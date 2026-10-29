//! Formatting helpers for dashboard UI rendering.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Row};

/// Truncate a string to max_len characters, appending ellipsis if truncated.
pub fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() > max_len {
        s.chars().take(max_len - 1).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

use crate::config::StatusIcons;
use crate::git::GitStatus;
use crate::github::{CheckSummary, PrSummary};
use crate::multiplexer::AgentStatus;
use crate::nerdfont;
use crate::nerdfont::GitIcons;
use crate::ui::pr_status::{PrStatusOptions, format_pr_details as shared_format_pr_details};
use crate::workflow::types::AgentStatusSummary;

use super::super::ansi;
use super::super::spinner::SPINNER_FRAMES;
use super::theme::ThemePalette;

/// Spacing mode for agent status icon rendering.
pub enum AgentStatusFormat {
    /// Table cell: trailing space after each icon, spinner padded with spaces.
    TableCell,
    /// Detail line: single space separators between present statuses only.
    DetailLine,
}

struct AgentStatusCounts {
    /// Working agents with an active turn (background agents excluded).
    working: usize,
    background: usize,
    waiting: usize,
    done: usize,
}

fn count_agent_statuses(summary: &AgentStatusSummary) -> AgentStatusCounts {
    let working = summary
        .statuses
        .iter()
        .filter(|s| **s == AgentStatus::Working)
        .count();
    // Background agents are stored as Working; split them out.
    let background = summary.background.min(working);
    AgentStatusCounts {
        working: working - background,
        background,
        waiting: summary
            .statuses
            .iter()
            .filter(|s| **s == AgentStatus::Waiting)
            .count(),
        done: summary
            .statuses
            .iter()
            .filter(|s| **s == AgentStatus::Done)
            .count(),
    }
}

/// Format agent status icons for dashboard table cells and detail lines.
pub fn format_agent_status_summary(
    summary: Option<&AgentStatusSummary>,
    icons: &StatusIcons,
    spinner_frame: u8,
    palette: &ThemePalette,
    mode: AgentStatusFormat,
) -> Vec<(String, Style)> {
    let Some(summary) = summary else {
        return if matches!(mode, AgentStatusFormat::TableCell) {
            vec![("-".to_string(), Style::default().fg(palette.dimmed))]
        } else {
            Vec::new()
        };
    };

    let counts = count_agent_statuses(summary);
    let spinner = SPINNER_FRAMES[spinner_frame as usize % SPINNER_FRAMES.len()];
    let separator_style = Style::default().fg(palette.text);
    let mut parts: Vec<(String, Style)> = Vec::new();
    let mut has_prior = false;

    if counts.working > 0 {
        let icon = icons.working();
        let base_style = Style::default().fg(palette.info);
        parts.extend(ansi::parse_tmux_styles(icon, base_style));
        match mode {
            AgentStatusFormat::TableCell => {
                parts.push((format!(" {} ", spinner), base_style));
            }
            AgentStatusFormat::DetailLine => {
                parts.push((format!(" {}", spinner), base_style));
            }
        }
        has_prior = true;
    }
    if counts.background > 0 {
        if matches!(mode, AgentStatusFormat::DetailLine) && has_prior {
            parts.push((" ".to_string(), separator_style));
        }
        // No spinner: the spinner means an active turn.
        let icon = icons.background();
        let base_style = Style::default().fg(palette.info);
        parts.extend(ansi::parse_tmux_styles(icon, base_style));
        if matches!(mode, AgentStatusFormat::TableCell) {
            parts.push((" ".to_string(), base_style));
        }
        has_prior = true;
    }
    if counts.waiting > 0 {
        if matches!(mode, AgentStatusFormat::DetailLine) && has_prior {
            parts.push((" ".to_string(), separator_style));
        }
        let icon = icons.waiting();
        let base_style = Style::default().fg(palette.accent);
        parts.extend(ansi::parse_tmux_styles(icon, base_style));
        if matches!(mode, AgentStatusFormat::TableCell) {
            parts.push((" ".to_string(), base_style));
        }
        has_prior = true;
    }
    if counts.done > 0 {
        if matches!(mode, AgentStatusFormat::DetailLine) && has_prior {
            parts.push((" ".to_string(), separator_style));
        }
        let icon = icons.done();
        let base_style = Style::default().fg(palette.success);
        parts.extend(ansi::parse_tmux_styles(icon, base_style));
        if matches!(mode, AgentStatusFormat::TableCell) {
            parts.push((" ".to_string(), base_style));
        }
    }

    if parts.is_empty() && matches!(mode, AgentStatusFormat::TableCell) {
        parts.push(("-".to_string(), Style::default().fg(palette.dimmed)));
    }

    parts
}

/// Add diff icon and uncommitted (+N/-N) spans. Caller is responsible for
/// the leading separator if needed.
fn add_uncommitted_spans(
    spans: &mut Vec<(String, Style)>,
    status: &GitStatus,
    icons: &GitIcons,
    palette: &ThemePalette,
) {
    spans.push((icons.diff.to_string(), Style::default().fg(palette.accent)));

    if status.uncommitted_added > 0 {
        spans.push((" ".to_string(), Style::default()));
        spans.push((
            format!("+{}", status.uncommitted_added),
            Style::default().fg(palette.success),
        ));
    }
    if status.uncommitted_removed > 0 {
        spans.push((" ".to_string(), Style::default()));
        spans.push((
            format!("-{}", status.uncommitted_removed),
            Style::default().fg(palette.danger),
        ));
    }
}

/// Format git status for the Git column: base branch, diff stats, then indicators
/// Format: "→branch +N -M 󰏫 +X -Y 󰀪 ↑A ↓B"
/// When there are uncommitted changes that differ from total, branch totals are dimmed
pub fn format_git_status(
    status: Option<&GitStatus>,
    spinner_frame: u8,
    palette: &ThemePalette,
) -> Vec<(String, Style)> {
    let icons = nerdfont::git_icons();

    if let Some(status) = status {
        // Not a repository at all: nothing to report, and not "dirty".
        if status.outside_repository {
            return vec![("?".to_string(), Style::default().fg(palette.dimmed))];
        }
        let mut spans: Vec<(String, Style)> = Vec::new();
        let has_uncommitted =
            status.uncommitted_added > 0 || status.uncommitted_removed > 0 || status.is_dirty;

        // Check if uncommitted equals total (all changes are uncommitted, nothing committed yet)
        let all_uncommitted = status.uncommitted_added == status.lines_added
            && status.uncommitted_removed == status.lines_removed;

        // Rebase indicator (shown first, before everything else)
        if status.is_rebasing {
            spans.push((
                icons.rebase.to_string(),
                Style::default().fg(palette.warning),
            ));
        }

        // Base branch (dimmed) - only show if not default (main/master)
        if !status.base_branch.is_empty()
            && status.base_branch != "main"
            && status.base_branch != "master"
        {
            spans.push((
                format!("→{}", status.base_branch),
                Style::default().fg(palette.dimmed),
            ));
        }

        // Always dim branch totals (historical), always bright uncommitted (active work)
        // - Clean: dim branch totals only
        // - All uncommitted: icon + bright uncommitted only
        // - Mixed: dim branch totals + icon + bright uncommitted
        if has_uncommitted && all_uncommitted {
            // All changes are uncommitted - show icon + bright numbers only
            if !spans.is_empty() {
                spans.push((" ".to_string(), Style::default()));
            }
            add_uncommitted_spans(&mut spans, status, &icons, palette);
        } else {
            // Either clean or mixed - show dim branch totals
            if status.lines_added > 0 {
                if !spans.is_empty() {
                    spans.push((" ".to_string(), Style::default()));
                }
                spans.push((
                    format!("+{}", status.lines_added),
                    Style::default()
                        .fg(palette.success)
                        .add_modifier(Modifier::DIM),
                ));
            }
            if status.lines_removed > 0 {
                if !spans.is_empty() {
                    spans.push((" ".to_string(), Style::default()));
                }
                spans.push((
                    format!("-{}", status.lines_removed),
                    Style::default()
                        .fg(palette.danger)
                        .add_modifier(Modifier::DIM),
                ));
            }

            // If there are uncommitted changes, show icon + bright uncommitted
            if has_uncommitted {
                if !spans.is_empty() {
                    spans.push((" ".to_string(), Style::default()));
                }
                add_uncommitted_spans(&mut spans, status, &icons, palette);
            }
        }

        // Conflict indicator
        if status.has_conflict {
            if !spans.is_empty() {
                spans.push((" ".to_string(), Style::default()));
            }
            spans.push((
                icons.conflict.to_string(),
                Style::default().fg(palette.danger),
            ));
        }

        // Ahead/behind upstream
        if status.ahead > 0 {
            if !spans.is_empty() {
                spans.push((" ".to_string(), Style::default()));
            }
            spans.push((
                format!("↑{}", status.ahead),
                Style::default().fg(palette.info),
            ));
        }
        if status.behind > 0 {
            if !spans.is_empty() {
                spans.push((" ".to_string(), Style::default()));
            }
            spans.push((
                format!("↓{}", status.behind),
                Style::default().fg(palette.warning),
            ));
        }

        // A branch with commits but no upstream has never been pushed, and has
        // no ↑ to show: say so instead of looking in sync.
        if !status.has_upstream
            && status.branch.is_some()
            && (status.lines_added > 0 || status.lines_removed > 0)
        {
            if !spans.is_empty() {
                spans.push((" ".to_string(), Style::default()));
            }
            spans.push((
                "no upstream".to_string(),
                Style::default().fg(palette.warning),
            ));
        }

        if spans.is_empty() {
            vec![("-".to_string(), Style::default().fg(palette.dimmed))]
        } else {
            spans
        }
    } else {
        // No status yet - show spinner
        let frame = SPINNER_FRAMES[spinner_frame as usize % SPINNER_FRAMES.len()];
        vec![(frame.to_string(), Style::default().fg(palette.dimmed))]
    }
}

/// Format the issues GitHub reports as closed by a pull request.
pub fn format_pr_issues(pr: Option<&PrSummary>) -> String {
    let Some(pr) = pr else {
        return String::new();
    };
    let pr_repository = pr.url.as_deref().and_then(|url| {
        let path = url.split_once("://")?.1.split_once('/')?.1;
        let mut segments = path.split('/');
        Some(format!("{}/{}", segments.next()?, segments.next()?))
    });

    pr.closing_issues
        .iter()
        .map(|issue| {
            if pr_repository
                .as_deref()
                .is_none_or(|repository| repository.eq_ignore_ascii_case(&issue.repository))
            {
                format!("#{}", issue.number)
            } else {
                format!("{}#{}", issue.repository, issue.number)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Format GitHub PR and check status as styled spans for dashboard display
pub fn format_pr_status(
    pr: Option<&PrSummary>,
    checks: Option<&CheckSummary>,
    show_check_counts: bool,
    spinner_frame: u8,
    palette: &ThemePalette,
) -> Vec<(String, Style)> {
    crate::ui::pr_status::format_github_status(
        pr,
        checks,
        PrStatusOptions {
            include_number: true,
            show_check_counts,
            none_placeholder: Some("-"),
            is_stale: false,
        },
        spinner_frame,
        palette,
    )
}

/// Returns minimal PR detail spans for the preview title.
/// - Pending: "◷ 12m" (dimmed)
/// - Failure: "× lint-check" (danger color)
/// - Success/None: empty
pub fn format_pr_details(
    pr: &PrSummary,
    spinner_frame: u8,
    palette: &ThemePalette,
) -> Vec<ratatui::text::Span<'static>> {
    shared_format_pr_details(pr, spinner_frame, palette)
}

/// Shared git/PR fetch state for resource table headers.
pub(crate) struct ResourceHeaderState<'a> {
    pub palette: &'a ThemePalette,
    pub spinner_frame: u8,
    pub git_fetching: bool,
    pub pr_fetching: bool,
}

/// A header cell of an agent or worktree resource table.
pub(crate) enum ResourceHeaderCell {
    /// Plain label with no fetch state.
    Plain(&'static str),
    /// Plain label built at runtime, such as a provider detail label.
    Owned(String),
    /// `Git` label, spinning while git data is being fetched.
    Git,
    /// `PR` label, spinning while GitHub data is being fetched.
    Pr,
}

/// Build the header row for agent and worktree resource tables.
pub(crate) fn resource_table_header(
    state: ResourceHeaderState<'_>,
    columns: &[ResourceHeaderCell],
) -> Row<'static> {
    let header_style = Style::default().fg(state.palette.header).bold();
    let header_cells = columns.iter().map(|column| match column {
        ResourceHeaderCell::Plain(label) => Cell::from(*label).style(header_style),
        ResourceHeaderCell::Owned(label) => Cell::from(label.clone()).style(header_style),
        ResourceHeaderCell::Git => Cell::from(build_column_header(
            "Git",
            state.git_fetching,
            state.spinner_frame,
            state.palette,
        )),
        ResourceHeaderCell::Pr => Cell::from(build_column_header(
            "PR",
            state.pr_fetching,
            state.spinner_frame,
            state.palette,
        )),
    });

    Row::new(header_cells.collect::<Vec<_>>()).height(1)
}

/// Bordered panel block with dashboard header styling.
pub(crate) fn panel_block(
    title: impl Into<Line<'static>>,
    palette: &ThemePalette,
) -> Block<'static> {
    let title_style = Style::default()
        .fg(palette.header)
        .add_modifier(Modifier::BOLD);
    let border_style = Style::default().fg(palette.border);
    Block::bordered()
        .title(title)
        .title_style(title_style)
        .border_style(border_style)
}

/// Build a column header with optional spinner when data is being fetched.
pub fn build_column_header(
    name: &str,
    is_fetching: bool,
    spinner_frame: u8,
    palette: &ThemePalette,
) -> Line<'static> {
    if is_fetching {
        let frame = SPINNER_FRAMES[spinner_frame as usize % SPINNER_FRAMES.len()];
        Line::from(vec![
            Span::styled(
                format!("{} ", name),
                Style::default().fg(palette.header).bold(),
            ),
            Span::styled(frame.to_string(), Style::default().fg(palette.dimmed)),
        ])
    } else {
        Line::from(Span::styled(
            name.to_string(),
            Style::default().fg(palette.header).bold(),
        ))
    }
}

/// Convert vec of (string, style) pairs into a ratatui Line for table cell rendering.
pub fn spans_to_line(spans: Vec<(String, Style)>) -> Line<'static> {
    Line::from(
        spans
            .into_iter()
            .map(|(text, style)| Span::styled(text, style))
            .collect::<Vec<_>>(),
    )
}

/// Calculate column width from string items with min/max clamping.
pub fn calc_column_width(items: &[String], min: usize, max: usize, padding: usize) -> u16 {
    items
        .iter()
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(min)
        .clamp(min, max)
        .saturating_add(padding) as u16
}

/// Upper bound for the `project` cell.
///
/// Narrow panes keep the historical cap of 20; wider panes leave room for
/// longer names such as `group/repository` paths instead of cutting them off.
pub fn project_width_budget(area_width: u16) -> usize {
    (area_width as usize / 5).clamp(20, 40)
}

/// Color elapsed time by age without using the destructive-action color.
pub(crate) fn elapsed_time_style(secs: Option<u64>, palette: &ThemePalette) -> Style {
    match secs {
        None => Style::default().fg(palette.dimmed),
        Some(secs) if secs < 5 * 60 => Style::default().fg(palette.success),
        Some(secs) if secs < 60 * 60 => Style::default().fg(palette.warning),
        Some(_) => Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::DIM),
    }
}

/// Render zero-valued clock units with less emphasis than active units.
pub(crate) fn elapsed_time_line(
    duration: String,
    secs: Option<u64>,
    palette: &ThemePalette,
) -> Line<'static> {
    let base = elapsed_time_style(secs, palette);
    let inactive = base.add_modifier(Modifier::DIM);
    let mut parts = duration.split(':');
    let (Some(hours), Some(minutes), Some(seconds), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Line::styled(duration, inactive);
    };

    Line::from(vec![
        Span::styled(
            hours.to_string(),
            if hours == "00" { inactive } else { base },
        ),
        Span::styled(":", inactive),
        Span::styled(
            minutes.to_string(),
            if minutes == "00" { inactive } else { base },
        ),
        Span::styled(":", inactive),
        Span::styled(seconds.to_string(), base),
    ])
}

/// Create a style for a table row based on whether it's the current or main worktree.
pub fn make_row_style(is_current: bool, is_main: bool, palette: &ThemePalette) -> Style {
    if is_current {
        Style::default().fg(palette.current_worktree_fg)
    } else if is_main {
        Style::default().fg(palette.dimmed)
    } else {
        Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ThemeMode, ThemeScheme};
    use crate::github::ClosingIssue;

    #[test]
    fn summary_shows_background_icon_without_spinner() {
        let palette = ThemePalette::for_scheme(ThemeScheme::Default, ThemeMode::Dark);
        let icons = StatusIcons::default();
        let summary = AgentStatusSummary {
            statuses: vec![AgentStatus::Working, AgentStatus::Working],
            background: 1,
        };
        for mode in [AgentStatusFormat::TableCell, AgentStatusFormat::DetailLine] {
            let spans = format_agent_status_summary(Some(&summary), &icons, 0, &palette, mode);
            let count = |needle: &str| spans.iter().filter(|(t, _)| t == needle).count();
            assert_eq!(count(icons.working()), 1, "{spans:?}");
            assert_eq!(count(icons.background()), 1, "{spans:?}");
            let spinners = spans
                .iter()
                .filter(|(t, _)| SPINNER_FRAMES.iter().any(|f| t.contains(*f)))
                .count();
            assert_eq!(spinners, 1, "only the working agent spins: {spans:?}");
            let bg = spans
                .iter()
                .position(|(t, _)| t == icons.background())
                .unwrap();
            assert_eq!(spans[bg].1.fg, Some(palette.info));
            assert!(
                spans
                    .iter()
                    .position(|(t, _)| t == icons.working())
                    .unwrap()
                    < bg
            );
        }
    }

    #[test]
    fn summary_with_only_background_agents_has_no_working_icon() {
        let palette = ThemePalette::for_scheme(ThemeScheme::Default, ThemeMode::Dark);
        let icons = StatusIcons::default();
        let summary = AgentStatusSummary {
            statuses: vec![AgentStatus::Working, AgentStatus::Waiting],
            background: 1,
        };
        let spans = format_agent_status_summary(
            Some(&summary),
            &icons,
            0,
            &palette,
            AgentStatusFormat::DetailLine,
        );
        let text: String = spans.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(text, format!("{} {}", icons.background(), icons.waiting()));
    }

    #[test]
    fn project_width_budget_scales_with_the_pane() {
        // Narrow panes keep the historical cap of 20.
        assert_eq!(project_width_budget(80), 20);
        assert_eq!(project_width_budget(100), 20);
        // Wider panes fit longer names such as group/repository paths.
        assert_eq!(project_width_budget(150), 30);
        assert_eq!(project_width_budget(200), 40);
        assert_eq!(project_width_budget(400), 40);
    }

    #[test]
    fn pr_issues_qualify_only_external_repositories() {
        let pr = PrSummary {
            number: 42,
            title: "Feature".into(),
            state: "OPEN".into(),
            is_draft: false,
            checks: None,
            check_meta: None,
            url: Some("https://github.com/raine/workmux/pull/42".into()),
            closing_issues: vec![
                ClosingIssue {
                    number: 308,
                    repository: "Raine/Workmux".into(),
                },
                ClosingIssue {
                    number: 12,
                    repository: "acme/tracker".into(),
                },
            ],
        };

        assert_eq!(format_pr_issues(Some(&pr)), "#308 acme/tracker#12");
        assert_eq!(format_pr_issues(None), "");
    }

    fn git_text(status: &GitStatus) -> String {
        let palette = ThemePalette::for_scheme(ThemeScheme::Default, ThemeMode::Dark);
        format_git_status(Some(status), 0, &palette)
            .iter()
            .map(|(text, _)| text.as_str())
            .collect()
    }

    #[test]
    fn git_status_marks_a_branch_with_commits_and_no_upstream() {
        let palette = ThemePalette::for_scheme(ThemeScheme::Default, ThemeMode::Dark);
        let status = GitStatus {
            branch: Some("feat/x".into()),
            lines_added: 5,
            ..Default::default()
        };
        let spans = format_git_status(Some(&status), 0, &palette);
        let marker = spans.iter().find(|(text, _)| text == "no upstream");
        assert_eq!(
            marker.map(|(_, style)| style.fg),
            Some(Some(palette.warning)),
            "{spans:?}"
        );
        assert_eq!(git_text(&status), "+5 no upstream");
    }

    #[test]
    fn git_status_has_no_upstream_marker_without_commits_or_branch() {
        let fresh = GitStatus {
            branch: Some("feat/x".into()),
            ..Default::default()
        };
        assert_eq!(git_text(&fresh), "-");
        let detached = GitStatus {
            branch: None,
            lines_added: 5,
            ..Default::default()
        };
        assert_eq!(git_text(&detached), "+5");
    }

    #[test]
    fn git_status_with_upstream_shows_unpushed_commits() {
        let status = GitStatus {
            branch: Some("feat/x".into()),
            has_upstream: true,
            lines_added: 5,
            ahead: 2,
            ..Default::default()
        };
        assert_eq!(git_text(&status), "+5 \u{2191}2");
    }

    #[test]
    fn git_status_flags_uncommitted_changes_with_a_warning_sign() {
        let status = GitStatus {
            branch: Some("feat/x".into()),
            has_upstream: true,
            is_dirty: true,
            lines_added: 3,
            uncommitted_added: 3,
            ..Default::default()
        };
        assert_eq!(git_text(&status), "\u{26a0}\u{fe0f} +3");
    }

    #[test]
    fn git_status_outside_a_repository_is_a_dimmed_question_mark() {
        let palette = ThemePalette::for_scheme(ThemeScheme::Default, ThemeMode::Dark);
        let status = GitStatus {
            outside_repository: true,
            is_dirty: true,
            ..Default::default()
        };
        let spans = format_git_status(Some(&status), 0, &palette);
        assert_eq!(
            spans,
            vec![("?".to_string(), Style::default().fg(palette.dimmed))]
        );
    }

    #[test]
    fn elapsed_time_dims_inactive_clock_units() {
        let palette = ThemePalette::for_scheme(ThemeScheme::Default, ThemeMode::Dark);
        let line = elapsed_time_line("00:01:11".to_string(), Some(71), &palette);

        assert!(line.spans[0].style.add_modifier.contains(Modifier::DIM));
        assert!(!line.spans[2].style.add_modifier.contains(Modifier::DIM));
        assert!(!line.spans[4].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn elapsed_time_color_progresses_with_age() {
        let palette = ThemePalette::for_scheme(ThemeScheme::Default, ThemeMode::Dark);
        assert_eq!(elapsed_time_style(None, &palette).fg, Some(palette.dimmed));
        assert_eq!(
            elapsed_time_style(Some(5 * 60 - 1), &palette).fg,
            Some(palette.success)
        );
        assert_eq!(
            elapsed_time_style(Some(5 * 60), &palette).fg,
            Some(palette.warning)
        );
        assert_eq!(
            elapsed_time_style(Some(60 * 60 - 1), &palette).fg,
            Some(palette.warning)
        );
        let long_running = elapsed_time_style(Some(60 * 60), &palette);
        assert_eq!(long_running.fg, Some(palette.accent));
        assert!(long_running.add_modifier.contains(Modifier::DIM));
    }
}
