//! Worktree table rendering for the dashboard worktree view.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Cell, Paragraph, Row, Table},
};

use crate::config::WorktreeColumn;

use super::super::agent;
use super::super::app::App;
use super::super::worktree_info::{InfoDetail, InfoEntry, InfoStatus, OrphanEntry, Tone};
use super::format;
use super::format::{
    AgentStatusFormat, ResourceHeaderCell, format_agent_status_summary, format_git_status,
    format_pr_status, truncate,
};
use super::theme::ThemePalette;

/// Render the worktree table in the given area.
pub fn render_worktree_table(f: &mut Frame, app: &mut App, area: Rect) {
    let show_env_column = app.worktree_info.is_some();

    // Don't render headers for an empty table - avoids a visual blink
    // as column widths jump when data arrives on the next frame. Orphan rows
    // are selectable, so they keep the table drawn.
    if !has_rows(
        app.worktrees.len(),
        visible_orphan_count(app, show_env_column),
    ) {
        return;
    }

    let show_check_counts = app.config.dashboard.show_check_counts();

    let worktree_max_width = worktree_width_budget(area.width);

    // Show the GitHub column when at least one worktree has a PR or checks.
    let show_pr_column = app.worktrees.iter().any(|worktree| {
        worktree.pr_info.is_some() || app.get_checks_for_worktree(worktree).is_some()
    });

    let columns = visible_columns(
        &app.config.dashboard.worktree_columns(),
        show_pr_column,
        show_env_column,
    );
    let show_pr_column = columns.contains(&WorktreeColumn::Pr);

    // Pre-compute row data
    let mut row_data: Vec<_> = app
        .worktrees
        .iter()
        .enumerate()
        .map(|(idx, wt)| {
            let jump_key = if idx < 9 {
                format!("{}", idx + 1)
            } else {
                String::new()
            };

            let project = app.project_for(&wt.repo_key, &wt.path);

            // Main worktree: show branch name (handle is just the repo dir name)
            // Other worktrees: show branch inline when it differs from the handle
            let worktree_display = if wt.is_main {
                wt.branch.clone()
            } else if wt.branch != wt.handle {
                format!("{} \u{2192}{}", wt.handle, wt.branch)
            } else {
                wt.handle.clone()
            };
            let worktree_display = if app.removals.contains(&wt.path) {
                let frames = super::super::spinner::SPINNER_FRAMES;
                let spinner = frames[app.spinner_frame as usize % frames.len()];
                format!(
                    "{spinner} Deleting {}",
                    truncate(&worktree_display, worktree_max_width.saturating_sub(11))
                )
            } else {
                truncate(&worktree_display, worktree_max_width)
            };

            // Git status
            let git_status = app.git_statuses.get(&wt.path);
            let git_spans = format_git_status(git_status, app.spinner_frame, &app.palette);

            let pr_title = wt
                .pr_info
                .as_ref()
                .map(|pr| pr.title.clone())
                .unwrap_or_default();
            let pr_issues = format::format_pr_issues(wt.pr_info.as_ref());

            // PR status (only computed if column is shown)
            let pr_spans = if show_pr_column {
                format_pr_status(
                    wt.pr_info.as_ref(),
                    app.get_checks_for_worktree(wt),
                    show_check_counts,
                    app.spinner_frame,
                    &app.palette,
                )
            } else {
                Vec::new()
            };

            // Agent status summary
            let agent_spans = format_agent_status_summary(
                wt.agent_status.as_ref(),
                &app.config.status_icons,
                app.spinner_frame,
                &app.palette,
                AgentStatusFormat::TableCell,
            );

            let is_current = app.current_worktree.as_ref().is_some_and(|cwd| {
                if let (Ok(cwd_canonical), Ok(wt_canonical)) =
                    (cwd.canonicalize(), wt.path.canonicalize())
                {
                    cwd_canonical == wt_canonical
                } else {
                    wt.path == *cwd
                }
            });

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let age = wt
                .created_at
                .map(|ts| agent::format_age(now.saturating_sub(ts)));

            let info = app.worktree_info_index.by_path.get(&wt.path);
            let (env_text, env_style) = env_cell(info, &app.palette);

            WorktreeRowData {
                jump_key,
                project,
                worktree_display,
                is_main: wt.is_main,
                is_current,
                git_spans,
                pr_spans,
                pr_title,
                pr_issues,
                agent_line: format::spans_to_line(agent_spans),
                has_mux_window: wt.has_mux_window,
                age: age.unwrap_or_default(),
                env_text,
                env_style,
                env_id: info.and_then(|entry| entry.id.clone()),
                env_details: info
                    .map(|entry| detail_pairs(&entry.details))
                    .unwrap_or_default(),
                base_spans: base_spans(git_status, &app.palette),
                is_orphan: false,
            }
        })
        .collect();

    // Orphan rows (environments whose worktree is gone) follow the real rows.
    if show_env_column {
        row_data.extend(
            app.visible_orphans
                .iter()
                .map(|orphan| orphan_row(orphan, &app.palette, worktree_max_width)),
        );
    }

    let columns = drop_empty_env_columns(columns, &row_data);
    let table = build_worktree_table(
        &columns,
        row_data,
        area.width,
        worktree_max_width,
        format::project_width_budget(area.width),
        format::ResourceHeaderState {
            palette: &app.palette,
            spinner_frame: app.spinner_frame,
            git_fetching: app
                .is_git_fetching
                .load(std::sync::atomic::Ordering::Relaxed),
            pr_fetching: app.is_pr_fetching(),
        },
    );
    f.render_stateful_widget(table, area, &mut app.worktree_table_state);
}

/// Orphan rows drawn for the current view (none without a provider report).
fn visible_orphan_count(app: &App, show_env_column: bool) -> usize {
    if show_env_column {
        app.visible_orphans.len()
    } else {
        0
    }
}

/// Whether the table has anything to draw: worktree rows or orphan rows.
fn has_rows(worktree_count: usize, orphan_count: usize) -> bool {
    worktree_count + orphan_count > 0
}

struct WorktreeRowData {
    jump_key: String,
    project: String,
    worktree_display: String,
    is_main: bool,
    is_current: bool,
    git_spans: Vec<(String, Style)>,
    pr_spans: Vec<(String, Style)>,
    pr_title: String,
    pr_issues: String,
    agent_line: Line<'static>,
    has_mux_window: bool,
    age: String,
    env_text: String,
    env_style: Style,
    env_id: Option<String>,
    /// Provider details as (label, value), in report order.
    env_details: Vec<(String, String)>,
    base_spans: Vec<(String, Style)>,
    is_orphan: bool,
}

/// Longest value shown in an `env:<label>` cell; the info panel keeps it whole.
const ENV_DETAIL_MAX_WIDTH: usize = 32;

fn detail_pairs(details: &[InfoDetail]) -> Vec<(String, String)> {
    details
        .iter()
        .map(|detail| (detail.label.clone(), detail.value.clone()))
        .collect()
}

/// Values reported under `label`, joined when the provider repeats it.
fn env_detail_value(row: &WorktreeRowData, label: &str) -> Option<String> {
    let values: Vec<&str> = row
        .env_details
        .iter()
        .filter(|(name, _)| name == label)
        .map(|(_, value)| value.as_str())
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// Header of an `env:<label>` column: short lowercase labels read as
/// abbreviations (`CPU`), longer ones are capitalized (`Disk`).
fn env_detail_header(label: &str) -> String {
    if label.chars().count() <= 3 && label.chars().all(|c| c.is_ascii_lowercase()) {
        return label.to_ascii_uppercase();
    }
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Base cell: the base branch (`main` when unknown, as in the info panel) and
/// the commits ahead and behind, colored like the panel.
fn base_spans(
    status: Option<&crate::git::GitStatus>,
    palette: &ThemePalette,
) -> Vec<(String, Style)> {
    let Some(status) = status else {
        return Vec::new();
    };
    let base = if status.base_branch.is_empty() {
        "main".to_string()
    } else {
        status.base_branch.clone()
    };
    let mut spans = vec![(base, Style::default().fg(palette.text))];
    if status.ahead > 0 {
        spans.push((
            format!(" \u{2191}{}", status.ahead),
            Style::default().fg(palette.info),
        ));
    }
    if status.behind > 0 {
        spans.push((
            format!(" \u{2193}{}", status.behind),
            Style::default().fg(palette.accent),
        ));
    }
    spans
}

/// Map a provider tone to a palette color.
fn tone_color(tone: &Tone, palette: &ThemePalette) -> Color {
    match tone {
        Tone::Success => palette.success,
        Tone::Warning => palette.warning,
        Tone::Error => palette.danger,
        Tone::Info => palette.info,
        Tone::Muted => palette.dimmed,
    }
}

fn status_cell(status: &InfoStatus, palette: &ThemePalette) -> (String, Style) {
    (
        format!("\u{25cf} {}", status.label),
        Style::default().fg(tone_color(&status.tone, palette)),
    )
}

/// Env cell: the colored status, or a dimmed `-` for a worktree without entry.
fn env_cell(entry: Option<&InfoEntry>, palette: &ThemePalette) -> (String, Style) {
    match entry {
        Some(entry) => status_cell(&entry.status, palette),
        None => ("-".into(), Style::default().fg(palette.dimmed)),
    }
}

const ORPHAN_SUFFIX: &str = " (missing)";

/// Row for an environment whose worktree no longer exists: only the project,
/// the `(missing)` name and the status are filled in.
fn orphan_row(
    orphan: &OrphanEntry,
    palette: &ThemePalette,
    name_max_width: usize,
) -> WorktreeRowData {
    let (env_text, env_style) = status_cell(&orphan.status, palette);
    let name = truncate(
        &orphan.name,
        name_max_width.saturating_sub(ORPHAN_SUFFIX.len()),
    );
    WorktreeRowData {
        jump_key: String::new(),
        project: orphan.project.clone(),
        worktree_display: format!("{name}{ORPHAN_SUFFIX}"),
        is_main: false,
        is_current: false,
        git_spans: Vec::new(),
        pr_spans: Vec::new(),
        pr_title: String::new(),
        pr_issues: String::new(),
        agent_line: Line::default(),
        has_mux_window: false,
        age: String::new(),
        env_text,
        env_style,
        env_id: orphan.id.clone(),
        env_details: detail_pairs(&orphan.details),
        base_spans: Vec::new(),
        is_orphan: true,
    }
}

/// Hide PR and Env until their data is available. A hidden column is dropped
/// unless that would blank the table.
fn visible_columns(
    configured: &[WorktreeColumn],
    show_pr_column: bool,
    show_env_column: bool,
) -> Vec<WorktreeColumn> {
    let visible: Vec<_> = configured
        .iter()
        .filter(|column| **column != WorktreeColumn::Pr || show_pr_column)
        .filter(|column| **column != WorktreeColumn::Env || show_env_column)
        .cloned()
        .collect();
    if visible.is_empty() {
        configured.to_vec()
    } else {
        visible
    }
}

/// Drop the Id and `env:<label>` columns that no row has a value for.
fn drop_empty_env_columns(
    columns: Vec<WorktreeColumn>,
    rows: &[WorktreeRowData],
) -> Vec<WorktreeColumn> {
    columns
        .into_iter()
        .filter(|column| match column {
            WorktreeColumn::EnvId => rows.iter().any(|row| row.env_id.is_some()),
            WorktreeColumn::EnvDetail(label) => rows
                .iter()
                .any(|row| row.env_details.iter().any(|(name, _)| name == label)),
            _ => true,
        })
        .collect()
}

/// Columns the table can lose when the pane is too narrow for all of them.
fn is_extra_column(column: &WorktreeColumn) -> bool {
    matches!(
        column,
        WorktreeColumn::Base | WorktreeColumn::EnvId | WorktreeColumn::EnvDetail(_)
    )
}

fn env_id_cell(row: &WorktreeRowData, palette: &ThemePalette) -> Cell<'static> {
    Cell::from(row.env_id.as_deref().map(short_id).unwrap_or_default())
        .style(Style::default().fg(palette.dimmed))
}

fn env_detail_cell(row: &WorktreeRowData, label: &str, style: Style) -> Cell<'static> {
    let value = env_detail_value(row, label).unwrap_or_default();
    Cell::from(truncate(&value, ENV_DETAIL_MAX_WIDTH)).style(style)
}

fn worktree_cell(
    column: &WorktreeColumn,
    row: &WorktreeRowData,
    palette: &ThemePalette,
) -> Cell<'static> {
    if row.is_orphan {
        return orphan_cell(column, row, palette);
    }
    match column {
        WorktreeColumn::Number => {
            Cell::from(row.jump_key.clone()).style(Style::default().fg(palette.keycap))
        }
        WorktreeColumn::Project => Cell::from(row.project.clone()),
        WorktreeColumn::Worktree => Cell::from(row.worktree_display.clone())
            .style(format::make_row_style(row.is_current, row.is_main, palette)),
        WorktreeColumn::Git => Cell::from(format::spans_to_line(row.git_spans.clone())),
        WorktreeColumn::Pr => Cell::from(format::spans_to_line(row.pr_spans.clone())),
        WorktreeColumn::PrTitle => Cell::from(row.pr_title.clone()),
        WorktreeColumn::PrIssues => Cell::from(row.pr_issues.clone()),
        WorktreeColumn::Base => Cell::from(format::spans_to_line(row.base_spans.clone())),
        WorktreeColumn::Env => Cell::from(row.env_text.clone()).style(row.env_style),
        WorktreeColumn::EnvId => env_id_cell(row, palette),
        WorktreeColumn::EnvDetail(label) => {
            env_detail_cell(row, label, Style::default().fg(palette.text))
        }
        WorktreeColumn::Mux => {
            if row.has_mux_window {
                Cell::from("\u{25cf}").style(Style::default().fg(palette.success))
            } else {
                Cell::from("-").style(Style::default().fg(palette.dimmed))
            }
        }
        WorktreeColumn::Age => {
            Cell::from(row.age.clone()).style(Style::default().fg(palette.dimmed))
        }
        WorktreeColumn::Agent => Cell::from(row.agent_line.clone()),
    }
}

/// Cells of an orphan row: dimmed, `(missing)` in the error color, the status
/// in Env and everything else empty.
fn orphan_cell(
    column: &WorktreeColumn,
    row: &WorktreeRowData,
    palette: &ThemePalette,
) -> Cell<'static> {
    let dimmed = Style::default().fg(palette.dimmed);
    match column {
        WorktreeColumn::Project => Cell::from(row.project.clone()).style(dimmed),
        WorktreeColumn::Worktree => {
            let name = row
                .worktree_display
                .strip_suffix(ORPHAN_SUFFIX)
                .unwrap_or(&row.worktree_display)
                .to_string();
            Cell::from(Line::from(vec![
                Span::styled(name, dimmed),
                Span::styled(ORPHAN_SUFFIX, Style::default().fg(palette.danger)),
            ]))
        }
        WorktreeColumn::Env => Cell::from(row.env_text.clone()).style(row.env_style),
        WorktreeColumn::EnvId => env_id_cell(row, palette),
        WorktreeColumn::EnvDetail(label) => env_detail_cell(row, label, dimmed),
        _ => Cell::from(""),
    }
}

/// Upper bound for the `worktree` cell.
///
/// The cell can carry both the handle and the branch, which a fixed cap cut off
/// even in a very wide terminal. Half the table leaves room for every other
/// column; the lower bound keeps narrow panes rendering as they always have,
/// and the upper one stops a single long branch from crowding the rest out.
fn worktree_width_budget(area_width: u16) -> usize {
    (area_width as usize / 2).clamp(25, 80)
}

/// Derive headers, cells and constraints from the same visible column order.
/// When the columns are wider than `area_width`, the extra columns (Base, Id
/// and `env:<label>`) are dropped from the end of the list until the rest fits.
fn build_worktree_table(
    columns: &[WorktreeColumn],
    row_data: Vec<WorktreeRowData>,
    area_width: u16,
    worktree_max_width: usize,
    project_max_width: usize,
    header_state: format::ResourceHeaderState<'_>,
) -> Table<'static> {
    let palette = header_state.palette;

    let project_names: Vec<String> = row_data.iter().map(|r| r.project.clone()).collect();
    let max_project_width = format::calc_column_width(&project_names, 5, project_max_width, 2);
    let worktree_names: Vec<String> = row_data
        .iter()
        .map(|r| r.worktree_display.clone())
        .collect();
    let max_worktree_width = format::calc_column_width(&worktree_names, 8, worktree_max_width, 1);
    let spans_width = |spans: &[(String, Style)]| -> usize {
        spans.iter().map(|(text, _)| text.chars().count()).sum()
    };
    let max_git_width = row_data
        .iter()
        .map(|row| spans_width(&row.git_spans))
        .max()
        .unwrap_or(4)
        .clamp(4, 30)
        + 1;
    let max_pr_width = row_data
        .iter()
        .map(|row| spans_width(&row.pr_spans))
        .max()
        .unwrap_or(4)
        .clamp(4, 16)
        + 1;
    let pr_titles: Vec<String> = row_data.iter().map(|row| row.pr_title.clone()).collect();
    let max_pr_title_width = format::calc_column_width(&pr_titles, 8, 60, 1);
    let pr_issues: Vec<String> = row_data.iter().map(|row| row.pr_issues.clone()).collect();
    let max_pr_issues_width = format::calc_column_width(&pr_issues, 9, 40, 1);
    let max_env_width = row_data
        .iter()
        .map(|row| row.env_text.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(3, 24)
        + 1;
    let max_base_width = row_data
        .iter()
        .map(|row| spans_width(&row.base_spans))
        .max()
        .unwrap_or(0)
        .clamp(4, 30)
        + 1;
    let max_agent_width = row_data
        .iter()
        .map(|row| row.agent_line.width())
        .max()
        .unwrap_or(0)
        .clamp(5, u16::MAX as usize) as u16;
    let env_detail_width = |label: &str| -> u16 {
        let widest = row_data
            .iter()
            .filter_map(|row| env_detail_value(row, label))
            .map(|value| value.chars().count().min(ENV_DETAIL_MAX_WIDTH))
            .max()
            .unwrap_or(0);
        (widest.max(env_detail_header(label).chars().count()) + 1) as u16
    };
    let width = |column: &WorktreeColumn| -> u16 {
        match column {
            WorktreeColumn::Number => 2,
            WorktreeColumn::Project => max_project_width,
            WorktreeColumn::Worktree => max_worktree_width,
            WorktreeColumn::Git => max_git_width as u16,
            WorktreeColumn::Pr => max_pr_width as u16,
            WorktreeColumn::PrTitle => max_pr_title_width,
            WorktreeColumn::PrIssues => max_pr_issues_width,
            WorktreeColumn::Base => max_base_width as u16,
            WorktreeColumn::Env => max_env_width as u16,
            WorktreeColumn::EnvId => 9,
            WorktreeColumn::EnvDetail(label) => env_detail_width(label),
            WorktreeColumn::Mux => 4,
            WorktreeColumn::Age => 4,
            WorktreeColumn::Agent => max_agent_width,
        }
    };

    // Two cells for the highlight symbol, one between columns.
    let total = |columns: &[WorktreeColumn]| -> usize {
        2 + columns.iter().map(|c| width(c) as usize).sum::<usize>()
            + columns.len().saturating_sub(1)
    };
    let mut columns = columns.to_vec();
    while total(&columns) > area_width as usize {
        let Some(last_extra) = columns.iter().rposition(is_extra_column) else {
            break;
        };
        columns.remove(last_extra);
    }
    let columns = columns.as_slice();

    let header_cells: Vec<_> = columns
        .iter()
        .map(|column| match column {
            WorktreeColumn::Number => ResourceHeaderCell::Plain("#"),
            WorktreeColumn::Project => ResourceHeaderCell::Plain("Project"),
            WorktreeColumn::Worktree => ResourceHeaderCell::Plain("Worktree"),
            WorktreeColumn::Git => ResourceHeaderCell::Git,
            WorktreeColumn::Pr => ResourceHeaderCell::Pr,
            WorktreeColumn::PrTitle => ResourceHeaderCell::Plain("PR Title"),
            WorktreeColumn::PrIssues => ResourceHeaderCell::Plain("PR Issues"),
            WorktreeColumn::Base => ResourceHeaderCell::Plain("Base"),
            WorktreeColumn::Env => ResourceHeaderCell::Plain("Env"),
            WorktreeColumn::EnvId => ResourceHeaderCell::Plain("Id"),
            WorktreeColumn::EnvDetail(label) => ResourceHeaderCell::Owned(env_detail_header(label)),
            WorktreeColumn::Mux => ResourceHeaderCell::Plain("Mux"),
            WorktreeColumn::Age => ResourceHeaderCell::Plain("Age"),
            WorktreeColumn::Agent => ResourceHeaderCell::Plain("Agent"),
        })
        .collect();

    let last_column = columns.len().saturating_sub(1);
    let constraints: Vec<_> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| match column {
            // Fill only at the end, so the columns before stay contiguous.
            WorktreeColumn::PrTitle | WorktreeColumn::PrIssues | WorktreeColumn::Agent
                if index == last_column =>
            {
                Constraint::Fill(1)
            }
            column => Constraint::Length(width(column)),
        })
        .collect();

    let rows: Vec<_> = row_data
        .into_iter()
        .map(|data| {
            let cells: Vec<_> = columns
                .iter()
                .map(|column| worktree_cell(column, &data, palette))
                .collect();
            let row = Row::new(cells);
            if data.is_current {
                row.style(Style::default().bg(palette.current_row_bg))
            } else {
                row
            }
        })
        .collect();

    let highlight_symbol = Text::from(Line::from(Span::styled(
        "▌ ",
        Style::default().fg(palette.info),
    )));
    Table::new(rows, constraints)
        .header(format::resource_table_header(header_state, &header_cells))
        .block(Block::default())
        .row_highlight_style(Style::default().bg(palette.highlight_row_bg))
        .highlight_symbol(highlight_symbol)
}

/// Render the worktree preview: info panel (left) + styled git log (right).
pub fn render_worktree_preview(f: &mut Frame, app: &mut App, area: Rect) {
    let selected_worktree = app
        .worktree_table_state
        .selected()
        .and_then(|idx| app.worktrees.get(idx));
    let selected_orphan = app.selected_orphan();

    // Split preview area into info panel (left) and git log (right)
    let chunks = Layout::horizontal([
        Constraint::Length(40), // Info panel: fixed width
        Constraint::Fill(1),    // Git log: remaining space
    ])
    .split(area);

    if let Some(orphan) = selected_orphan {
        render_orphan_info_panel(f, app, chunks[0], orphan);
    } else {
        render_info_panel(f, app, chunks[0], selected_worktree);
    }
    render_git_log(
        f,
        app,
        chunks[1],
        selected_worktree,
        selected_orphan.is_some(),
    );
}

/// Title of the info panel. With workspaces configured a main checkout's
/// handle is just the repository directory name, so it is shown as
/// `project · branch`; otherwise the title is the handle (upstream).
fn info_title(
    wt: &crate::workflow::types::WorktreeInfo,
    project: &str,
    workspaces_configured: bool,
) -> String {
    if wt.is_main && workspaces_configured {
        format!(" {} \u{b7} {} ", project, wt.branch)
    } else {
        format!(" {} ", wt.handle)
    }
}

fn env_status_line<'a>(
    status: &'a InfoStatus,
    id: Option<&str>,
    palette: &ThemePalette,
) -> Line<'a> {
    let mut spans = vec![
        Span::styled("Env     ", Style::default().fg(palette.dimmed)),
        Span::styled(
            status.label.as_str(),
            Style::default().fg(tone_color(&status.tone, palette)),
        ),
    ];
    if let Some(id) = id {
        spans.push(Span::styled(
            format!(" \u{b7} {}", id.chars().take(8).collect::<String>()),
            Style::default().fg(palette.dimmed),
        ));
    }
    Line::from(spans)
}

fn detail_lines<'a>(details: &'a [InfoDetail], palette: &ThemePalette) -> Vec<Line<'a>> {
    details
        .iter()
        .map(|detail| {
            Line::from(vec![
                Span::styled(
                    format!("{:<7} ", detail.label),
                    Style::default().fg(palette.dimmed),
                ),
                Span::styled(detail.value.as_str(), Style::default().fg(palette.text)),
            ])
        })
        .collect()
}

/// Env section of the info panel for a real worktree with an entry.
fn env_info_lines<'a>(entry: &'a InfoEntry, palette: &ThemePalette) -> Vec<Line<'a>> {
    let mut lines = vec![env_status_line(&entry.status, entry.id.as_deref(), palette)];
    lines.extend(detail_lines(&entry.details, palette));
    lines
}

/// Info panel content for an orphan row.
fn orphan_info_lines<'a>(orphan: &'a OrphanEntry, palette: &ThemePalette) -> Vec<Line<'a>> {
    let label = Style::default().fg(palette.dimmed);
    let text = Style::default().fg(palette.text);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Project ", label),
            Span::styled(orphan.project.as_str(), text),
        ]),
        Line::from(vec![
            Span::styled("Path    ", label),
            Span::styled(orphan.path.to_string_lossy().into_owned(), text),
        ]),
        env_status_line(&orphan.status, orphan.id.as_deref(), palette),
    ];
    lines.extend(detail_lines(&orphan.details, palette));
    if let Some(hint) = &orphan.hint {
        lines.push(Line::styled(hint.as_str(), label));
    }
    lines
}

fn render_orphan_info_panel(f: &mut Frame, app: &App, area: Rect, orphan: &OrphanEntry) {
    let block = format::panel_block(format!(" {}{ORPHAN_SUFFIX} ", orphan.name), &app.palette);
    let paragraph =
        Paragraph::new(Text::from(orphan_info_lines(orphan, &app.palette))).block(block);
    f.render_widget(paragraph, area);
}

/// Render the info panel showing worktree metadata.
fn render_info_panel(
    f: &mut Frame,
    app: &App,
    area: Rect,
    worktree: Option<&crate::workflow::types::WorktreeInfo>,
) {
    let label_style = Style::default().fg(app.palette.dimmed);
    let text_style = Style::default().fg(app.palette.text);

    let title = if let Some(wt) = worktree {
        info_title(
            wt,
            &app.project_for(&wt.repo_key, &wt.path),
            !app.config.dashboard.workspaces().is_empty(),
        )
    } else {
        " Info ".to_string()
    };

    let block = format::panel_block(title, &app.palette);

    let Some(wt) = worktree else {
        let paragraph = Paragraph::new(Text::raw("(no worktree selected)")).block(block);
        f.render_widget(paragraph, area);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();

    // Branch
    lines.push(Line::from(vec![
        Span::styled("Branch  ", label_style),
        Span::styled(&wt.branch, text_style),
    ]));

    // Git status details (base branch, ahead/behind, diff stats)
    let git_status = app.git_statuses.get(&wt.path);
    if let Some(status) = git_status {
        // Base branch + ahead/behind
        let mut base_spans = vec![Span::styled("Base    ", label_style)];
        if !status.base_branch.is_empty() {
            base_spans.push(Span::styled(&status.base_branch, text_style));
        } else {
            base_spans.push(Span::styled("main", text_style));
        }
        if status.ahead > 0 || status.behind > 0 {
            base_spans.push(Span::styled(" (", label_style));
            if status.ahead > 0 {
                base_spans.push(Span::styled(
                    format!("\u{2191}{}", status.ahead),
                    Style::default().fg(app.palette.info),
                ));
            }
            if status.ahead > 0 && status.behind > 0 {
                base_spans.push(Span::styled(" ", label_style));
            }
            if status.behind > 0 {
                base_spans.push(Span::styled(
                    format!("\u{2193}{}", status.behind),
                    Style::default().fg(app.palette.accent),
                ));
            }
            base_spans.push(Span::styled(")", label_style));
        }
        lines.push(Line::from(base_spans));

        // Committed diff stats
        if status.lines_added > 0 || status.lines_removed > 0 {
            let mut diff_spans = vec![Span::styled("Diff    ", label_style)];
            if status.lines_added > 0 {
                diff_spans.push(Span::styled(
                    format!("+{}", status.lines_added),
                    Style::default().fg(app.palette.success),
                ));
            }
            if status.lines_added > 0 && status.lines_removed > 0 {
                diff_spans.push(Span::styled(" ", text_style));
            }
            if status.lines_removed > 0 {
                diff_spans.push(Span::styled(
                    format!("-{}", status.lines_removed),
                    Style::default().fg(app.palette.danger),
                ));
            }
            diff_spans.push(Span::styled(" committed", label_style));
            lines.push(Line::from(diff_spans));
        }

        // Uncommitted changes
        if status.uncommitted_added > 0 || status.uncommitted_removed > 0 {
            let mut uc_spans = vec![Span::styled("        ", label_style)];
            if status.uncommitted_added > 0 {
                uc_spans.push(Span::styled(
                    format!("+{}", status.uncommitted_added),
                    Style::default().fg(app.palette.success),
                ));
            }
            if status.uncommitted_added > 0 && status.uncommitted_removed > 0 {
                uc_spans.push(Span::styled(" ", text_style));
            }
            if status.uncommitted_removed > 0 {
                uc_spans.push(Span::styled(
                    format!("-{}", status.uncommitted_removed),
                    Style::default().fg(app.palette.danger),
                ));
            }
            uc_spans.push(Span::styled(" uncommitted", label_style));
            lines.push(Line::from(uc_spans));
        }

        // Rebase indicator
        if status.is_rebasing {
            let git_icons = crate::nerdfont::git_icons();
            lines.push(Line::from(vec![
                Span::styled("        ", label_style),
                Span::styled(
                    format!("{} ", git_icons.rebase),
                    Style::default().fg(app.palette.warning),
                ),
                Span::styled("rebase in progress", label_style),
            ]));
        }

        // Conflict indicator
        if status.has_conflict {
            lines.push(Line::from(vec![
                Span::styled("        ", label_style),
                Span::styled(
                    "conflict with base",
                    Style::default().fg(app.palette.danger),
                ),
            ]));
        }
    }

    // PR info
    if let Some(ref pr) = wt.pr_info {
        let pr_icons = crate::nerdfont::pr_icons();
        let (icon, color) = if pr.is_draft {
            (pr_icons.draft, app.palette.dimmed)
        } else {
            match pr.state.as_str() {
                "OPEN" => (pr_icons.open, app.palette.success),
                "MERGED" => (pr_icons.merged, app.palette.accent),
                "CLOSED" => (pr_icons.closed, app.palette.danger),
                _ => ("?", app.palette.dimmed),
            }
        };
        let mut pr_spans = vec![
            Span::styled("PR      ", label_style),
            Span::styled(format!("#{} ", pr.number), Style::default().fg(color)),
            Span::styled(icon, Style::default().fg(color)),
        ];
        // Check status
        if let Some(ref checks) = pr.checks {
            use crate::github::CheckState;
            let check_icons = crate::nerdfont::check_icons();
            let (check_icon, check_color) = match checks {
                CheckState::Success => (check_icons.success.to_string(), app.palette.success),
                CheckState::Failure { .. } => (check_icons.failure.to_string(), app.palette.danger),
                CheckState::Pending { .. } => (check_icons.pending.to_string(), app.palette.accent),
            };
            pr_spans.push(Span::styled(" ", text_style));
            pr_spans.push(Span::styled(check_icon, Style::default().fg(check_color)));
        }
        lines.push(Line::from(pr_spans));

        // PR title (truncated to fit)
        let inner_width = area.width.saturating_sub(2) as usize; // border
        let title_max = inner_width.saturating_sub(8); // label width
        let truncated_title = if pr.title.chars().count() > title_max {
            format!(
                "{}...",
                pr.title
                    .chars()
                    .take(title_max.saturating_sub(3))
                    .collect::<String>()
            )
        } else {
            pr.title.clone()
        };
        lines.push(Line::from(vec![
            Span::styled("        ", label_style),
            Span::styled(truncated_title, Style::default().fg(color)),
        ]));

        // Check detail: failing check name or pending elapsed time
        let detail_spans = super::format::format_pr_details(pr, app.spinner_frame, &app.palette);
        if !detail_spans.is_empty() {
            let mut line_spans = vec![Span::styled("        ", label_style)];
            line_spans.extend(detail_spans);
            lines.push(Line::from(line_spans));
        }
    }

    // Agent status
    if wt.agent_status.is_some() {
        let mut agent_spans = vec![Span::styled("Agent   ", label_style)];
        for (text, style) in format_agent_status_summary(
            wt.agent_status.as_ref(),
            &app.config.status_icons,
            app.spinner_frame,
            &app.palette,
            AgentStatusFormat::DetailLine,
        ) {
            agent_spans.push(Span::styled(text, style));
        }
        lines.push(Line::from(agent_spans));
    }

    // Mux window
    let mux_spans = vec![
        Span::styled("Mux     ", label_style),
        if wt.has_mux_window {
            Span::styled("\u{25cf} active", Style::default().fg(app.palette.success))
        } else {
            Span::styled("- none", Style::default().fg(app.palette.dimmed))
        },
    ];
    lines.push(Line::from(mux_spans));

    // Env (external provider)
    if let Some(entry) = app.worktree_info_index.by_path.get(&wt.path) {
        lines.extend(env_info_lines(entry, &app.palette));
    }

    let paragraph = Paragraph::new(Text::from(lines)).block(block);
    f.render_widget(paragraph, area);
}

/// Render the styled git log panel.
fn render_git_log(
    f: &mut Frame,
    app: &App,
    area: Rect,
    worktree: Option<&crate::workflow::types::WorktreeInfo>,
    is_orphan: bool,
) {
    let block = format::panel_block(" Git Log ", &app.palette);
    let text = git_log_text(
        app.worktree_preview.as_deref(),
        worktree.is_some(),
        is_orphan,
        &app.palette,
    );
    let paragraph = Paragraph::new(text).block(block);
    f.render_widget(paragraph, area);
}

fn git_log_text<'a>(
    preview: Option<&'a str>,
    has_worktree: bool,
    is_orphan: bool,
    palette: &ThemePalette,
) -> Text<'a> {
    if is_orphan {
        return Text::styled("(worktree missing)", Style::default().fg(palette.dimmed));
    }
    match (preview, has_worktree) {
        (Some(log), true) if !log.trim().is_empty() => {
            let hash_style = Style::default().fg(palette.accent);
            let date_style = Style::default().fg(palette.dimmed);
            let msg_style = Style::default().fg(palette.text);

            let lines: Vec<Line> = log
                .lines()
                .map(|line| {
                    let parts: Vec<&str> = line.splitn(3, '\t').collect();
                    if parts.len() == 3 {
                        Line::from(vec![
                            Span::styled(parts[0], hash_style),
                            Span::styled("  ", date_style),
                            Span::styled(parts[1], date_style),
                            Span::styled("  ", msg_style),
                            Span::styled(parts[2], msg_style),
                        ])
                    } else {
                        // Fallback for lines that don't match format
                        Line::styled(line, msg_style)
                    }
                })
                .collect();
            Text::from(lines)
        }
        (None, true) => Text::raw(""),
        (Some(_), true) => Text::raw("(no commits)"),
        (_, false) => Text::raw(""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DEFAULT_WORKTREE_COLUMNS, ThemeConfig, ThemeMode};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, widgets::TableState};

    fn palette() -> ThemePalette {
        ThemePalette::from_config(&ThemeConfig::default(), ThemeMode::Dark)
    }

    fn row() -> WorktreeRowData {
        WorktreeRowData {
            jump_key: "1".into(),
            project: "proj".into(),
            worktree_display: "wt".into(),
            is_main: false,
            is_current: false,
            git_spans: vec![("+1".into(), Style::default())],
            pr_spans: vec![("#7".into(), Style::default())],
            pr_title: "fix the dashboard".into(),
            pr_issues: "#308 #270".into(),
            agent_line: Line::from("working"),
            has_mux_window: true,
            age: "2h".into(),
            env_text: "-".into(),
            env_style: Style::default(),
            env_id: None,
            env_details: Vec::new(),
            base_spans: Vec::new(),
            is_orphan: false,
        }
    }

    fn with_details(id: Option<&str>, details: &[(&str, &str)]) -> WorktreeRowData {
        let mut data = row();
        data.env_id = id.map(Into::into);
        data.env_details = details
            .iter()
            .map(|(label, value)| ((*label).into(), (*value).into()))
            .collect();
        data
    }

    fn info_status(label: &str, tone: Tone) -> InfoStatus {
        InfoStatus {
            label: label.into(),
            tone,
        }
    }

    fn entry(label: &str) -> InfoEntry {
        InfoEntry {
            path: "/ws/alpha/wt".into(),
            status: info_status(label, Tone::Success),
            id: Some("dd4b4587-0000-4000-8000-000000000000".into()),
            details: vec![
                InfoDetail {
                    label: "backend".into(),
                    value: "http://127.0.0.1:18600".into(),
                },
                InfoDetail {
                    label: "console".into(),
                    value: "http://dev-host.example:18601".into(),
                },
            ],
        }
    }

    fn orphan() -> OrphanEntry {
        OrphanEntry {
            project: "alpha".into(),
            name: "gone".into(),
            path: "/ws/alpha/gone".into(),
            status: info_status("unknown", Tone::Warning),
            id: Some("dd4b4587-0000-4000-8000-000000000000".into()),
            details: vec![],
            hint: Some("my-env-tool forget dd4b4587".into()),
        }
    }

    fn flat(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn render(
        columns: &[WorktreeColumn],
        rows: Vec<WorktreeRowData>,
        width: u16,
        selected: Option<usize>,
    ) -> Buffer {
        let palette = palette();
        let table = build_worktree_table(
            columns,
            rows,
            width,
            worktree_width_budget(width),
            format::project_width_budget(width),
            format::ResourceHeaderState {
                palette: &palette,
                spinner_frame: 0,
                git_fetching: false,
                pr_fetching: false,
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(width, 4)).unwrap();
        let mut state = TableState::default().with_selected(selected);
        terminal
            .draw(|f| f.render_stateful_widget(table, f.area(), &mut state))
            .unwrap();
        assert_eq!(state.selected(), selected);
        terminal.backend().buffer().clone()
    }

    #[test]
    fn worktree_table_uses_project_map() {
        use WorktreeColumn::*;
        let mut names = std::collections::HashMap::new();
        names.insert(std::path::PathBuf::from("k/.git"), "alpha".to_string());
        let mut r = row();
        r.project = crate::command::dashboard::app::resolve_project_name(
            &names,
            std::path::Path::new("k/.git"),
            std::path::Path::new("/ws/zzz/primary-clone"),
        );
        let buffer = render(&[Worktree, Project], vec![r], 40, None);
        assert_eq!(line(&buffer, 0), "Worktree  Project");
        assert_eq!(line(&buffer, 1), "wt        alpha");
    }

    #[test]
    fn info_title_for_main_checkout() {
        let mut wt = crate::workflow::types::WorktreeInfo {
            repo_key: "k/.git".into(),
            handle: "primary-clone".into(),
            branch: "dev".into(),
            path: "/ws/alpha/primary-clone".into(),
            is_main: true,
            mode: crate::config::MuxMode::Window,
            has_mux_window: false,
            has_unmerged: false,
            pr_info: None,
            agent_status: None,
            created_at: None,
            base_branch: None,
        };
        assert!(info_title(&wt, "alpha", true).contains("alpha \u{b7} dev"));
        wt.is_main = false;
        assert_eq!(info_title(&wt, "alpha", true), " primary-clone ");
    }

    #[test]
    fn info_title_without_workspaces_is_the_handle() {
        let wt = crate::workflow::types::WorktreeInfo {
            repo_key: "k/.git".into(),
            handle: "primary-clone".into(),
            branch: "dev".into(),
            path: "/ws/alpha/primary-clone".into(),
            is_main: true,
            mode: crate::config::MuxMode::Window,
            has_mux_window: false,
            has_unmerged: false,
            pr_info: None,
            agent_status: None,
            created_at: None,
            base_branch: None,
        };
        assert_eq!(info_title(&wt, "alpha", false), " primary-clone ");
    }

    #[test]
    fn orphans_alone_keep_the_table_drawn() {
        assert!(!has_rows(0, 0));
        assert!(has_rows(0, 1));
        assert!(has_rows(2, 0));
        // Orphans only (no worktree rows) still produce a table with a row.
        let palette = palette();
        let rows = vec![orphan_row(&orphan(), &palette, 25)];
        let buffer = render(
            &[WorktreeColumn::Worktree, WorktreeColumn::Env],
            rows,
            40,
            Some(0),
        );
        assert!(
            line(&buffer, 1).contains("gone (missing)"),
            "{:?}",
            line(&buffer, 1)
        );
    }

    fn line(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn worktree_table_preserves_default_layout() {
        let columns = visible_columns(&DEFAULT_WORKTREE_COLUMNS, true, false);
        let buffer = render(&columns, vec![row()], 80, None);
        assert_eq!(
            line(&buffer, 0),
            "#  Project Worktree  Git   PR    Mux  Age  Agent"
        );
        assert_eq!(
            line(&buffer, 1),
            "1  proj    wt        +1    #7    ●    2h   working"
        );
    }

    #[test]
    fn worktree_table_reorders_headers_and_cells() {
        use WorktreeColumn::*;
        let buffer = render(
            &[Agent, Age, Mux, Pr, Git, Worktree, Project, Number],
            vec![row()],
            80,
            None,
        );
        assert_eq!(
            line(&buffer, 0),
            "Agent   Age  Mux  PR    Git   Worktree  Project #"
        );
        assert_eq!(
            line(&buffer, 1),
            "working 2h   ●    #7    +1    wt        proj    1"
        );
    }

    #[test]
    fn worktree_table_omits_unlisted_columns_at_narrow_width() {
        use WorktreeColumn::*;
        let buffer = render(&[Worktree, Agent], vec![row()], 24, None);
        assert_eq!(line(&buffer, 0), "Worktree  Agent");
        assert_eq!(line(&buffer, 1), "wt        working");
    }

    #[test]
    fn worktree_table_renders_opt_in_pr_metadata_without_pr_status() {
        use WorktreeColumn::*;
        let buffer = render(&[PrTitle, PrIssues], vec![row()], 60, None);
        assert_eq!(line(&buffer, 0), "PR Title           PR Issues");
        assert_eq!(line(&buffer, 1), "fix the dashboard  #308 #270");
    }

    #[test]
    fn agent_width_uses_display_cells_and_fits_header() {
        use WorktreeColumn::*;
        for (agent, expected) in [("", "Agent Mux"), ("作業中です", "Agent      Mux")] {
            let mut data = row();
            data.agent_line = Line::from(agent);
            let buffer = render(&[Agent, Mux], vec![data], 40, None);
            assert_eq!(line(&buffer, 0), expected);
        }
    }

    #[test]
    fn trailing_agent_fills_after_pr_is_hidden() {
        use WorktreeColumn::*;
        let columns = visible_columns(&[Agent, Pr], false, false);
        assert_eq!(columns, [Agent]);
        let mut data = row();
        data.agent_line = Line::from("working").right_aligned();
        let buffer = render(&columns, vec![data], 20, None);
        assert_eq!(line(&buffer, 1), "             working");
    }

    #[test]
    fn pr_visibility_preserves_order_and_pr_only_fallback() {
        use WorktreeColumn::*;
        assert_eq!(
            visible_columns(&[Agent, Pr, Worktree], false, false),
            [Agent, Worktree]
        );
        assert_eq!(
            visible_columns(&[Agent, Pr, Worktree], true, false),
            [Agent, Pr, Worktree]
        );
        assert_eq!(visible_columns(&[Pr], false, false), [Pr]);
        let mut data = row();
        data.pr_spans = format_pr_status(None, None, false, 0, &palette());
        let buffer = render(&visible_columns(&[Pr], false, false), vec![data], 20, None);
        assert_eq!(line(&buffer, 0), "PR");
        assert_eq!(line(&buffer, 1), "-");
    }

    #[test]
    fn reordered_table_preserves_row_styles_and_selection() {
        use WorktreeColumn::*;
        let mut current = row();
        current.is_current = true;
        let mut main = row();
        main.is_main = true;
        main.has_mux_window = false;
        let palette = palette();
        let buffer = render(&[Mux, Worktree], vec![current, main], 30, Some(1));
        assert_eq!(buffer[(7, 1)].fg, palette.current_worktree_fg);
        assert_eq!(buffer[(7, 1)].bg, palette.current_row_bg);
        assert_eq!(buffer[(7, 2)].fg, palette.dimmed);
        assert_eq!(buffer[(7, 2)].bg, palette.highlight_row_bg);
        assert_eq!(buffer[(2, 1)].fg, palette.success);
        assert_eq!(buffer[(2, 2)].fg, palette.dimmed);
        assert_eq!(buffer[(0, 2)].symbol(), "▌");
    }

    #[test]
    fn worktree_width_budget_scales_with_the_pane() {
        // Narrow panes keep the historical fixed width.
        assert_eq!(worktree_width_budget(40), 25);
        assert_eq!(worktree_width_budget(50), 25);
        // Wider panes get proportionally more, up to a ceiling.
        assert_eq!(worktree_width_budget(120), 60);
        assert_eq!(worktree_width_budget(200), 80);
        assert_eq!(worktree_width_budget(400), 80);
    }

    #[test]
    fn project_column_fits_a_long_name_in_a_wide_pane() {
        let long = "group-of-repos/repository";
        let mut r = row();
        r.project = long.into();
        let buffer = render(&[WorktreeColumn::Project], vec![r], 200, None);
        assert!(
            line(&buffer, 1).contains(long),
            "long project name should survive in a wide pane, got {:?}",
            line(&buffer, 1)
        );
    }

    #[test]
    fn worktree_column_uses_the_extra_width_in_a_wide_pane() {
        let long = "feat/some-rather-long-branch-name";
        let mut r = row();
        r.worktree_display = long.into();
        let buffer = render(&[WorktreeColumn::Worktree], vec![r], 120, None);
        assert!(
            line(&buffer, 1).contains(long),
            "long worktree label should survive in a wide pane, got {:?}",
            line(&buffer, 1)
        );
    }

    #[test]
    fn env_column_hidden_without_report() {
        use WorktreeColumn::*;
        assert_eq!(
            visible_columns(&[Worktree, Env, Mux], false, false),
            [Worktree, Mux]
        );
        assert_eq!(
            visible_columns(&[Worktree, Env, Mux], false, true),
            [Worktree, Env, Mux]
        );
        let columns = visible_columns(&DEFAULT_WORKTREE_COLUMNS, true, false);
        let buffer = render(&columns, vec![row()], 80, None);
        assert!(!line(&buffer, 0).contains("Env"));
    }

    #[test]
    fn env_column_shows_status() {
        use WorktreeColumn::*;
        let palette = palette();
        let mut with_env = row();
        (with_env.env_text, with_env.env_style) = env_cell(Some(&entry("ready")), &palette);
        let mut without = row();
        (without.env_text, without.env_style) = env_cell(None, &palette);
        let buffer = render(&[Worktree, Env], vec![with_env, without], 40, None);
        assert_eq!(line(&buffer, 0), "Worktree  Env");
        assert_eq!(line(&buffer, 1), "wt        \u{25cf} ready");
        assert_eq!(line(&buffer, 2), "wt        -");
        assert_eq!(buffer[(10, 1)].fg, palette.success);
        assert_eq!(buffer[(10, 2)].fg, palette.dimmed);
    }

    #[test]
    fn orphan_rows_render_after_worktrees() {
        use WorktreeColumn::*;
        let palette = palette();
        let rows = vec![row(), row(), orphan_row(&orphan(), &palette, 25)];
        let buffer = render(&[Number, Project, Worktree, Env], rows, 60, None);
        let l = line(&buffer, 3);
        assert!(l.contains("alpha"), "{l:?}");
        assert!(l.contains("gone (missing)"), "{l:?}");
        assert!(l.contains("unknown"), "{l:?}");
        assert_eq!(buffer[(0, 3)].symbol(), " ");
        assert_eq!(buffer[(1, 3)].symbol(), " ");
        let x = l.find("(missing)").unwrap() as u16;
        assert_eq!(buffer[(x, 3)].fg, palette.danger);
        let y = l.find("gone").unwrap() as u16;
        assert_eq!(buffer[(y, 3)].fg, palette.dimmed);
    }

    #[test]
    fn info_panel_env_lines() {
        let binding = entry("ready");
        let lines = env_info_lines(&binding, &palette());
        let text = flat(&lines);
        assert!(
            text.contains(&"Env     ready \u{b7} dd4b4587".to_string()),
            "{text:?}"
        );
        assert!(
            text.contains(&"backend http://127.0.0.1:18600".to_string()),
            "{text:?}"
        );
        assert!(
            text.contains(&"console http://dev-host.example:18601".to_string()),
            "{text:?}"
        );
    }

    #[test]
    fn info_panel_orphan() {
        let palette = palette();
        let orphan = orphan();
        let text = flat(&orphan_info_lines(&orphan, &palette));
        assert_eq!(text[0], "Project alpha");
        assert_eq!(text[1], "Path    /ws/alpha/gone");
        assert_eq!(text[2], "Env     unknown \u{b7} dd4b4587");
        assert_eq!(text.last().unwrap(), "my-env-tool forget dd4b4587");
        let log = git_log_text(None, false, true, &palette);
        assert_eq!(log.lines[0].spans[0].content, "(worktree missing)");
    }

    #[test]
    fn tone_colors_follow_the_palette() {
        let p = palette();
        assert_eq!(tone_color(&Tone::Success, &p), p.success);
        assert_eq!(tone_color(&Tone::Warning, &p), p.warning);
        assert_eq!(tone_color(&Tone::Error, &p), p.danger);
        assert_eq!(tone_color(&Tone::Info, &p), p.info);
        assert_eq!(tone_color(&Tone::Muted, &p), p.dimmed);
    }

    #[test]
    fn env_detail_columns_show_values_under_their_label() {
        use WorktreeColumn::*;
        let rows = vec![
            with_details(
                Some("dd4b4587-0000"),
                &[("cpu", "3.5%"), ("disk", "4.8 GiB")],
            ),
            with_details(None, &[("disk", "2.1 GiB")]),
        ];
        let columns = [
            Worktree,
            EnvId,
            EnvDetail("cpu".into()),
            EnvDetail("disk".into()),
        ];
        let buffer = render(&columns, rows, 60, None);
        let header = line(&buffer, 0);
        let first = line(&buffer, 1);
        let second = line(&buffer, 2);
        assert_eq!(
            header.split_whitespace().collect::<Vec<_>>(),
            ["Worktree", "Id", "CPU", "Disk"]
        );
        assert_eq!(
            first.split_whitespace().collect::<Vec<_>>(),
            ["wt", "dd4b4587", "3.5%", "4.8", "GiB"]
        );
        assert_eq!(header.find("Disk"), first.find("4.8"));
        assert_eq!(header.find("Disk"), second.find("2.1"));
        assert_eq!(
            second.split_whitespace().collect::<Vec<_>>(),
            ["wt", "2.1", "GiB"]
        );
    }

    #[test]
    fn env_detail_column_joins_repeated_labels_and_truncates_long_values() {
        use WorktreeColumn::*;
        let long = "http://orkestra-staging-1.tailc4dad.ts.net:18601";
        let rows = vec![with_details(
            None,
            &[("stack", "a (2)"), ("stack", "b (1)"), ("console", long)],
        )];
        let buffer = render(
            &[EnvDetail("stack".into()), EnvDetail("console".into())],
            rows,
            120,
            None,
        );
        let first = line(&buffer, 1);
        assert!(first.starts_with("a (2), b (1)"), "{first:?}");
        assert!(
            first.contains("http://orkestra-staging-1.tailc…"),
            "{first:?}"
        );
        assert!(!first.contains(long), "{first:?}");
    }

    #[test]
    fn env_detail_headers_capitalize_the_label() {
        assert_eq!(env_detail_header("cpu"), "CPU");
        assert_eq!(env_detail_header("mem"), "MEM");
        assert_eq!(env_detail_header("disk"), "Disk");
        assert_eq!(env_detail_header("volumes"), "Volumes");
    }

    #[test]
    fn base_column_shows_branch_and_divergence() {
        let palette = palette();
        let status = crate::git::GitStatus {
            base_branch: "dev".into(),
            ahead: 2,
            behind: 7,
            ..Default::default()
        };
        let spans = base_spans(Some(&status), &palette);
        assert_eq!(
            spans
                .iter()
                .map(|(text, _)| text.as_str())
                .collect::<String>(),
            "dev \u{2191}2 \u{2193}7"
        );
        let plain = crate::git::GitStatus::default();
        assert_eq!(
            base_spans(Some(&plain), &palette)
                .iter()
                .map(|(text, _)| text.as_str())
                .collect::<String>(),
            "main"
        );
        assert!(base_spans(None, &palette).is_empty());
        let mut data = row();
        data.base_spans = spans;
        let buffer = render(
            &[WorktreeColumn::Worktree, WorktreeColumn::Base],
            vec![data],
            40,
            None,
        );
        assert_eq!(
            line(&buffer, 0).split_whitespace().collect::<Vec<_>>(),
            ["Worktree", "Base"]
        );
        assert!(
            line(&buffer, 1).ends_with("dev \u{2191}2 \u{2193}7"),
            "{:?}",
            line(&buffer, 1)
        );
    }

    #[test]
    fn env_columns_without_any_value_are_dropped() {
        use WorktreeColumn::*;
        let rows = vec![with_details(None, &[("cpu", "1.0%")])];
        assert_eq!(
            drop_empty_env_columns(
                vec![
                    Worktree,
                    EnvId,
                    EnvDetail("cpu".into()),
                    EnvDetail("mem".into()),
                    Base
                ],
                &rows
            ),
            [Worktree, EnvDetail("cpu".into()), Base]
        );
    }

    #[test]
    fn extra_columns_that_do_not_fit_are_dropped_from_the_end() {
        use WorktreeColumn::*;
        let rows = vec![with_details(
            Some("dd4b4587-0000"),
            &[
                ("cpu", "3.5%"),
                (
                    "console",
                    "http://orkestra-staging-1.tailc4dad.ts.net:18601",
                ),
            ],
        )];
        let columns = vec![
            Number,
            Worktree,
            EnvDetail("cpu".into()),
            Agent,
            EnvId,
            EnvDetail("console".into()),
        ];
        let buffer = render(&columns, rows, 40, None);
        let header = line(&buffer, 0);
        assert_eq!(
            header.split_whitespace().collect::<Vec<_>>(),
            ["#", "Worktree", "CPU", "Agent", "Id"],
            "{header:?}"
        );
        let wide = render(
            &columns,
            vec![with_details(
                Some("dd4b4587-0000"),
                &[("cpu", "3.5%"), ("console", "x")],
            )],
            80,
            None,
        );
        assert!(line(&wide, 0).ends_with("Console"), "{:?}", line(&wide, 0));
    }

    #[test]
    fn orphan_rows_fill_env_id_and_detail_columns() {
        use WorktreeColumn::*;
        let palette = palette();
        let mut gone = orphan();
        gone.details = vec![InfoDetail {
            label: "reason".into(),
            value: "worktree-missing".into(),
        }];
        let rows = vec![orphan_row(&gone, &palette, 25)];
        let buffer = render(
            &[Worktree, EnvId, EnvDetail("reason".into())],
            rows,
            60,
            None,
        );
        let first = line(&buffer, 1);
        assert!(first.contains("dd4b4587"), "{first:?}");
        assert!(first.contains("worktree-missing"), "{first:?}");
    }
}
