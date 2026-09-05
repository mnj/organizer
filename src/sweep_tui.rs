//! Beautiful fullscreen TUI checklist for Sweep include filtering (#27).
//!
//! Framework: [`ratatui`] with the crossterm backend — no hand-rolled ANSI.
//! The pure [`ToggleModel`] below holds all toggle semantics and is fully
//! testable without a terminal; [`run_include_tui`] is a thin ratatui shell
//! over it (all ON by default, Space/click toggles, Enter confirms).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::config::Action;

/// One Action row in the toggle screen, with its known-hash count for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionOption {
    pub action: Action,
    pub hash_count: usize,
}

/// Build display options from database Actions plus per-Action known-hash counts.
/// `counts` keys are folder names (any case); missing entries count as zero.
/// Order follows `actions`.
pub fn action_options(
    actions: &[Action],
    counts: &HashMap<String, usize>,
) -> Vec<ActionOption> {
    // Normalize count keys once so callers may pass any case.
    let normalized: HashMap<String, usize> = counts
        .iter()
        .map(|(k, &v)| (k.to_ascii_lowercase(), v))
        .fold(HashMap::new(), |mut acc, (k, v)| {
            *acc.entry(k).or_insert(0) += v;
            acc
        });
    actions
        .iter()
        .map(|a| {
            let count = normalized
                .get(&a.folder_name.to_ascii_lowercase())
                .copied()
                .unwrap_or(0);
            ActionOption {
                action: a.clone(),
                hash_count: count,
            }
        })
        .collect()
}

/// Count known hashes per origin Action folder (lower-cased) from file rows.
/// Powers the per-row "N known" badges in the TUI.
pub fn count_hashes_per_action(files: &[crate::store::FileRecord]) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for f in files {
        *out
            .entry(f.action_folder.to_ascii_lowercase())
            .or_insert(0) += 1;
    }
    out
}

/// Pure toggle model: which origin Actions count as Sweep matches.
/// All ON by default; toggling off excludes that origin's hashes.
/// No terminal here — fully unit-testable at the seam.
#[derive(Debug, Clone)]
pub struct ToggleModel {
    actions: Vec<Action>,
    enabled: Vec<bool>,
    cursor: usize,
}

impl ToggleModel {
    /// All Actions enabled, cursor on the first row.
    pub fn new(actions: &[Action]) -> Self {
        Self {
            actions: actions.to_vec(),
            enabled: vec![true; actions.len()],
            cursor: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.actions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    pub fn is_enabled(&self, index: usize) -> bool {
        self.enabled.get(index).copied().unwrap_or(false)
    }

    pub fn enabled_count(&self) -> usize {
        self.enabled.iter().filter(|&&b| b).count()
    }

    pub fn is_all_on(&self) -> bool {
        !self.enabled.is_empty() && self.enabled.iter().all(|&b| b)
    }

    /// Toggle the row under the cursor.
    pub fn toggle_cursor(&mut self) {
        if self.cursor < self.enabled.len() {
            self.enabled[self.cursor] = !self.enabled[self.cursor];
        }
    }

    /// Toggle an explicit row (mouse click / number key seam).
    pub fn toggle_index(&mut self, index: usize) {
        if index < self.enabled.len() {
            self.enabled[index] = !self.enabled[index];
        }
    }

    pub fn move_up(&mut self) {
        if self.actions.is_empty() {
            return;
        }
        self.cursor = if self.cursor == 0 {
            self.actions.len() - 1
        } else {
            self.cursor - 1
        };
    }

    pub fn move_down(&mut self) {
        if self.actions.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1) % self.actions.len();
    }

    pub fn move_to(&mut self, index: usize) {
        if index < self.actions.len() {
            self.cursor = index;
        }
    }

    pub fn select_all(&mut self) {
        for b in &mut self.enabled {
            *b = true;
        }
    }

    pub fn select_none(&mut self) {
        for b in &mut self.enabled {
            *b = false;
        }
    }

    /// Confirmed selection as a Sweep include filter:
    /// - `None` when every Action is ON (default: match all origins).
    /// - `Some(set)` otherwise (folder lower-cased allow-list; possibly empty
    ///   for "match nothing"). Unknown-hash files stay untouched either way.
    pub fn to_filter(&self) -> Option<HashSet<String>> {
        if self.is_all_on() {
            return None;
        }
        let mut out = HashSet::new();
        for (a, &on) in self.actions.iter().zip(self.enabled.iter()) {
            if on {
                out.insert(a.folder_name.to_ascii_lowercase());
            }
        }
        Some(out)
    }
}

/// Outcome of the fullscreen toggle screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiOutcome {
    /// User confirmed: drives the Sweep run (`None` = all origins).
    Confirmed(Option<HashSet<String>>),
    /// User cancelled (Esc/q/Ctrl-C): caller must not run the Sweep.
    Cancelled,
}

/// Run the beautiful fullscreen checklist with ratatui (crossterm backend).
///
/// - Lists every origin Action, all ON by default.
/// - Space/x toggles the highlighted row, click toggles any row, `a` = all,
///   `n` = none, `1`-`9` toggle by position, Enter confirms, Esc/q cancels.
/// - Returns the confirmed include filter with identical semantics to
///   `--include` (`None` = all origins).
pub fn run_include_tui(
    options: &[ActionOption],
    target: &Path,
    db_path: &Path,
) -> std::io::Result<TuiOutcome> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
    use crossterm::execute;
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    use ratatui::layout::{Constraint, Direction, Layout, Rect};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;
    use std::io::{self, Stdout};

    let actions: Vec<Action> = options.iter().map(|o| o.action.clone()).collect();
    if actions.is_empty() {
        return Ok(TuiOutcome::Confirmed(None));
    }
    let mut model = ToggleModel::new(&actions);

    enable_raw_mode()?;
    let mut stdout: Stdout = io::stdout();
    // Mouse click toggles rows; alternate screen keeps the report visible after exit.
    execute!(
        stdout,
        EnterAlternateScreen,
        event::EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Row rectangles for click-to-toggle hit-testing, refreshed each frame.
    let mut row_areas: Vec<Rect> = Vec::new();

    let outcome: std::io::Result<TuiOutcome> = (|| {
        loop {
            // ---- render ----
            terminal.draw(|frame| {
                let area = frame.area();
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3),
                        Constraint::Length(3),
                        Constraint::Min(4),
                        Constraint::Length(3),
                    ])
                    .split(area);

                let enabled = model.enabled_count();
                let total = model.len();
                let accent = Color::Rgb(137, 180, 250); // catppuccin blue
                let green = Color::Rgb(166, 227, 161); // catppuccin green
                let dim = Color::Rgb(108, 112, 134); // overlay

                let title = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(accent))
                    .title(Line::from(vec![
                        Span::styled(
                            " ◈ Sweep ",
                            Style::default().fg(accent).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            "— choose origin Actions ",
                            Style::default().fg(Color::White),
                        ),
                    ]));
                let title_inner = title.inner(chunks[0]);
                frame.render_widget(title, chunks[0]);
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(
                            format!(" {enabled}/{total} enabled "),
                            Style::default().fg(green).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            "· only hashes from enabled Actions count as matches",
                            Style::default().fg(dim),
                        ),
                    ])),
                    title_inner,
                );

                let info = Paragraph::new(vec![
                    Line::from(vec![
                        Span::styled("Target: ", Style::default().fg(dim)),
                        Span::raw(target.display().to_string()),
                    ]),
                    Line::from(vec![
                        Span::styled("Reference: ", Style::default().fg(dim)),
                        Span::raw(db_path.display().to_string()),
                    ]),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(dim))
                        .title(" Scope "),
                );
                frame.render_widget(info, chunks[1]);

                // Action rows.
                let mut items: Vec<ListItem> = Vec::with_capacity(options.len());
                for (i, opt) in options.iter().enumerate() {
                    let on = model.is_enabled(i);
                    let checkbox = if on { "●" } else { "○" };
                    let checkbox_style = if on {
                        Style::default().fg(green).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(dim)
                    };
                    let cursor_mark = if i == model.cursor() { "▶ " } else { "  " };
                    let count_label = if opt.hash_count == 1 {
                        "1 known".to_string()
                    } else {
                        format!("{} known", opt.hash_count)
                    };
                    let name_style = if on {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(dim)
                    };
                    items.push(
                        ListItem::new(Line::from(vec![
                            Span::styled(
                                cursor_mark.to_string(),
                                Style::default().fg(accent),
                            ),
                            Span::styled(
                                format!("{checkbox} "),
                                checkbox_style,
                            ),
                            Span::styled(opt.action.display_name.clone(), name_style),
                            Span::styled(
                                format!("  ({})", opt.action.folder_name),
                                Style::default().fg(dim),
                            ),
                            Span::styled(
                                format!("  [{}]", opt.action.shortcut),
                                Style::default().fg(accent),
                            ),
                            Span::styled(
                                format!("  · {count_label}"),
                                Style::default().fg(dim),
                            ),
                        ])),
                    );
                }
                let list_block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(accent))
                    .title(format!(" Origin Actions ({} rows) ", options.len()));
                let list = List::new(items)
                    .block(list_block)
                    .highlight_style(
                        Style::default()
                            .bg(Color::Rgb(49, 50, 68))
                            .add_modifier(Modifier::BOLD),
                    );
                // Reserve inner rect for hit-testing before rendering.
                let list_area = chunks[2];
                let inner = Rect {
                    x: list_area.x + 1,
                    y: list_area.y + 1,
                    width: list_area.width.saturating_sub(2),
                    height: list_area.height.saturating_sub(2),
                };
                row_areas.clear();
                for i in 0..options.len() {
                    if (i as u16) < inner.height {
                        row_areas.push(Rect {
                            x: inner.x,
                            y: inner.y + i as u16,
                            width: inner.width,
                            height: 1,
                        });
                    } else {
                        row_areas.push(Rect::default());
                    }
                }
                frame.render_widget(list, list_area);
                // Cursor highlight: render a background row behind the selected line.
                if model.cursor() < row_areas.len() {
                    let cursor_rect = row_areas[model.cursor()];
                    if cursor_rect.width > 0 && cursor_rect.height > 0 {
                        frame.render_widget(
                            Block::default().style(
                                Style::default().bg(Color::Rgb(49, 50, 68)),
                            ),
                            cursor_rect,
                        );
                        // Re-render cursor row text on top so it stays readable.
                        if let Some(opt) = options.get(model.cursor()) {
                            let on = model.is_enabled(model.cursor());
                            let checkbox = if on { "●" } else { "○" };
                            frame.render_widget(
                                Paragraph::new(Line::from(vec![
                                    Span::styled(
                                        "▶ ",
                                        Style::default().fg(accent),
                                    ),
                                    Span::styled(
                                        format!("{checkbox} "),
                                        if on {
                                            Style::default()
                                                .fg(green)
                                                .add_modifier(Modifier::BOLD)
                                        } else {
                                            Style::default().fg(dim)
                                        },
                                    ),
                                    Span::styled(
                                        opt.action.display_name.clone(),
                                        Style::default()
                                            .fg(Color::White)
                                            .add_modifier(Modifier::BOLD),
                                    ),
                                    Span::styled(
                                        format!("  ({})", opt.action.folder_name),
                                        Style::default().fg(dim),
                                    ),
                                    Span::styled(
                                        format!("  [{}]", opt.action.shortcut),
                                        Style::default().fg(accent),
                                    ),
                                ])),
                                cursor_rect,
                            );
                        }
                    }
                }

                let footer = Paragraph::new(Line::from(vec![
                    Span::styled(
                        " ↑↓/j/k ",
                        Style::default().fg(accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("move  ", Style::default().fg(Color::White)),
                    Span::styled(
                        "Space ",
                        Style::default().fg(accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("toggle  ", Style::default().fg(Color::White)),
                    Span::styled(
                        "a/n ",
                        Style::default().fg(accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("all/none  ", Style::default().fg(Color::White)),
                    Span::styled(
                        "Enter ",
                        Style::default().fg(green).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("confirm  ", Style::default().fg(Color::White)),
                    Span::styled(
                        "q/Esc ",
                        Style::default().fg(Color::Rgb(243, 139, 168)),
                    ),
                    Span::styled("cancel  ", Style::default().fg(Color::White)),
                    Span::styled("click toggles", Style::default().fg(dim)),
                ]))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(dim))
                        .title(" Keys "),
                );
                frame.render_widget(footer, chunks[3]);
            })?;

            // ---- events ----
            let ev = event::read()?;
            match ev {
                Event::Key(key) => match key.code {
                    KeyCode::Up | KeyCode::Char('k') => model.move_up(),
                    KeyCode::Down | KeyCode::Char('j') => model.move_down(),
                    KeyCode::Home => model.move_to(0),
                    KeyCode::End => model.move_to(model.len().saturating_sub(1)),
                    KeyCode::Char(' ') | KeyCode::Char('x') => model.toggle_cursor(),
                    KeyCode::Char('a') | KeyCode::Char('A') => model.select_all(),
                    KeyCode::Char('n') | KeyCode::Char('N') => model.select_none(),
                    KeyCode::Char(c) if ('1'..='9').contains(&c) => {
                        let idx = (c as usize) - ('1' as usize);
                        if idx < model.len() {
                            model.move_to(idx);
                            model.toggle_index(idx);
                        }
                    }
                    KeyCode::Enter => {
                        return Ok(TuiOutcome::Confirmed(model.to_filter()));
                    }
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                        return Ok(TuiOutcome::Cancelled);
                    }
                    KeyCode::Char('c') | KeyCode::Char('C')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        return Ok(TuiOutcome::Cancelled);
                    }
                    _ => {}
                },
                Event::Mouse(mouse) => {
                    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                        for (i, area) in row_areas.iter().enumerate() {
                            if mouse.column >= area.x
                                && mouse.column < area.x.saturating_add(area.width)
                                && mouse.row == area.y
                            {
                                model.move_to(i);
                                model.toggle_index(i);
                                break;
                            }
                        }
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    })();

    // ---- restore terminal ----
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        event::DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Action;

    fn sample_actions() -> Vec<Action> {
        vec![
            Action { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
            Action { display_name: "Maybe".into(), folder_name: "maybe".into(), shortcut: "2".into() },
            Action { display_name: "Reject".into(), folder_name: "reject".into(), shortcut: "3".into() },
        ]
    }

    #[test]
    fn new_model_has_all_on_and_none_filter() {
        let model = ToggleModel::new(&sample_actions());
        assert_eq!(model.len(), 3);
        assert_eq!(model.cursor(), 0);
        assert_eq!(model.enabled_count(), 3);
        assert!(model.is_all_on());
        assert_eq!(model.to_filter(), None, "all ON must mean default-all (None)");
    }

    #[test]
    fn toggling_off_excludes_and_toggling_on_reincludes() {
        let mut model = ToggleModel::new(&sample_actions());
        // Cursor starts on row 0 (keep): toggle it off.
        model.toggle_cursor();
        assert!(!model.is_enabled(0));
        assert!(model.is_enabled(1));
        let filter = model.to_filter().expect("partial selection must be Some");
        assert!(!filter.contains("keep"), "toggled-off origin must be excluded");
        assert!(filter.contains("maybe"));
        assert!(filter.contains("reject"));

        // Toggle back on: full selection returns to default-all.
        model.toggle_cursor();
        assert!(model.is_enabled(0));
        assert_eq!(model.to_filter(), None, "re-including must restore default-all");
    }

    #[test]
    fn cursor_movement_and_index_toggle_drive_confirmed_selection() {
        let mut model = ToggleModel::new(&sample_actions());
        model.move_down();
        assert_eq!(model.cursor(), 1);
        model.toggle_cursor(); // maybe off
        model.move_down();
        model.toggle_cursor(); // reject off
        let filter = model.to_filter().expect("must be Some");
        assert_eq!(filter, HashSet::from(["keep".to_string()]));

        // Mouse/number seam: toggle by explicit index.
        model.toggle_index(1); // maybe back on
        let filter = model.to_filter().expect("must be Some");
        assert!(filter.contains("keep"));
        assert!(filter.contains("maybe"));
        assert!(!filter.contains("reject"));
    }

    #[test]
    fn select_none_matches_nothing_select_all_restores_default() {
        let mut model = ToggleModel::new(&sample_actions());
        model.select_none();
        assert_eq!(model.enabled_count(), 0);
        assert_eq!(model.to_filter(), Some(HashSet::new()));
        model.select_all();
        assert!(model.is_all_on());
        assert_eq!(model.to_filter(), None);
    }

    #[test]
    fn move_wraps_around_for_fast_keyboard_triage() {
        let mut model = ToggleModel::new(&sample_actions());
        model.move_up(); // wrap from 0 to last
        assert_eq!(model.cursor(), 2);
        model.move_down(); // wrap back to 0
        assert_eq!(model.cursor(), 0);
    }

    #[test]
    fn action_options_carry_counts_case_insensitively() {
        let actions = sample_actions();
        let counts = HashMap::from([
            ("KEEP".to_string(), 4usize),
            ("maybe".to_string(), 1usize),
        ]);
        let opts = action_options(&actions, &counts);
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].hash_count, 4);
        assert_eq!(opts[1].hash_count, 1);
        assert_eq!(opts[2].hash_count, 0, "missing entries count as zero");
    }

    #[test]
    fn counts_group_by_lower_cased_origin_folder() {
        let files = vec![
            crate::store::FileRecord::new(
                &"aa".repeat(32), "a.jpg", "keep/a.jpg", "Keep", 1, 0, 0,
            )
            .unwrap(),
            crate::store::FileRecord::new(
                &"bb".repeat(32), "b.jpg", "keep/b.jpg", "KEEP", 1, 0, 0,
            )
            .unwrap(),
            crate::store::FileRecord::new(
                &"cc".repeat(32), "c.jpg", "maybe/c.jpg", "maybe", 1, 0, 0,
            )
            .unwrap(),
        ];
        let counts = count_hashes_per_action(&files);
        assert_eq!(counts.get("keep"), Some(&2));
        assert_eq!(counts.get("maybe"), Some(&1));
    }

    #[test]
    fn toggle_model_filter_matches_include_flag_resolution() {
        use crate::sweep::resolve_include_filter;
        let actions = sample_actions();
        // TUI: keep only row 0 (toggle the other two off).
        let mut model = ToggleModel::new(&actions);
        model.toggle_index(1);
        model.toggle_index(2);
        let via_toggle = model.to_filter().expect("must be Some");
        // Flag: --include keep resolves to the same folder allow-list.
        let via_flag = resolve_include_filter(&["keep".to_string()], &actions);
        assert_eq!(via_toggle, via_flag);
    }
}
