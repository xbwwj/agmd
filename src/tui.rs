use std::{
    ffi::OsStr,
    io::{self, Stdout},
    path::Path,
    process::Command,
    time::Duration,
};

use chrono::NaiveDate;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{
        EnterAlternateScreen,
        LeaveAlternateScreen,
        disable_raw_mode,
        enable_raw_mode,
    },
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block,
        HighlightSpacing,
        List,
        ListItem,
        ListState,
    },
};

use crate::parse::{Task, collect_tasks, strip_due_annotation};

// -----------------------------------------------------------------------------
// TUI lifecycle
// -----------------------------------------------------------------------------

/// 初始化终端、加载任务、运行 TUI，并在退出时恢复终端状态。
pub fn run_tui(root: &Path, today: NaiveDate) -> io::Result<()> {
    let mut tasks = load_tasks(root)?;

    enable_raw_mode()?;

    let mut stdout = io::stdout();

    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error);
    }

    let backend = CrosstermBackend::new(stdout);

    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            return Err(error);
        }
    };

    let result = run_app(
        &mut terminal,
        root,
        &mut tasks,
        today,
    );

    let cleanup_result = (|| {
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        Ok::<_, io::Error>(())
    })();

    match (result, cleanup_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

// -----------------------------------------------------------------------------
// Task loading and sorting
// -----------------------------------------------------------------------------

/// 重新扫描 Markdown 文件并按截止日期、路径和行号排序。
fn load_tasks(root: &Path) -> io::Result<Vec<Task>> {
    let mut tasks = collect_tasks(root)
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::Other,
                error.to_string(),
            )
        })?;

    tasks.sort_by(|a, b| {
        let due_order = match (a.due, b.due) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };

        due_order
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });

    Ok(tasks)
}

/// 重新加载任务。
///
/// 只有扫描成功后才替换旧列表，避免刷新失败时丢失当前数据。
fn reload_tasks(
    tasks: &mut Vec<Task>,
    root: &Path,
) -> io::Result<()> {
    let reloaded = load_tasks(root)?;

    *tasks = reloaded;

    Ok(())
}

/// 根据当前过滤条件修正选中索引。
fn select_visible_index(
    state: &mut ListState,
    tasks: &[Task],
    show_completed: bool,
    preferred_index: usize,
) {
    let visible_count = tasks
        .iter()
        .filter(|task| show_completed || !task.checked)
        .count();

    let selected = if visible_count == 0 {
        None
    } else {
        Some(preferred_index.min(visible_count - 1))
    };

    state.select(selected);
}

// -----------------------------------------------------------------------------
// TUI application
// -----------------------------------------------------------------------------

/// 运行任务列表的交互循环。
///
/// 默认隐藏已完成任务。
///
/// 快捷键：
///
/// - `↑` / `↓`、`j` / `k`：上下移动。
/// - `PageUp` / `PageDown`：分页移动。
/// - `Home` / `End`：跳转到首尾。
/// - `Enter`：使用外部编辑器打开当前任务所在文件，并定位到对应行；
///   编辑器退出后自动重新加载。
/// - `r`：重新扫描并加载任务。
/// - `t`：切换已完成任务的显示状态。
/// - `q` / `Esc` / `Ctrl-C`：退出。
fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    root: &Path,
    tasks: &mut Vec<Task>,
    today: NaiveDate,
) -> io::Result<()> {
    // 默认隐藏已完成任务。
    let mut show_completed = false;

    let mut state = ListState::default();

    select_visible_index(
        &mut state,
        tasks,
        show_completed,
        0,
    );

    loop {
        // 用索引构建可见列表，避免在处理按键时持续借用 tasks。
        let visible_indices: Vec<usize> = tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| show_completed || !task.checked)
            .map(|(index, _)| index)
            .collect();

        let row_count = visible_indices.len();

        let items = {
            let visible_tasks: Vec<&Task> = visible_indices
                .iter()
                .map(|&index| &tasks[index])
                .collect();

            build_list_items(&visible_tasks, today)
        };

        let completed_status = if show_completed {
            "completed: shown"
        } else {
            "completed: hidden"
        };

        let title = format!(
            " md-due · {} tasks · {} · Enter open · r reload · t toggle · ↑/↓ j/k navigate · PgUp/PgDn · Home/End · q quit ",
            row_count,
            completed_status,
        );

        terminal.draw(|frame| {
            let list = List::new(items)
                .block(Block::bordered().title(title.as_str()))
                .highlight_style(
                    Style::default().add_modifier(Modifier::REVERSED)
                )
                .highlight_symbol("▸ ")
                .highlight_spacing(HighlightSpacing::Always)
                .scroll_padding(1);

            frame.render_stateful_widget(
                list,
                frame.area(),
                &mut state,
            );
        })?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }

        let Event::Key(key) = event::read()? else {
            continue;
        };

        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,

            KeyCode::Char('c')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                break;
            }

            // -----------------------------------------------------------------
            // Enter: 打开当前任务所在文件
            // -----------------------------------------------------------------

            KeyCode::Enter => {
                let selected = state.selected().unwrap_or(0);

                if let Some(&task_index) = visible_indices.get(selected) {
                    open_task_in_editor(
                        terminal,
                        root,
                        &tasks[task_index],
                    )?;

                    // 编辑器退出后自动重新加载。
                    reload_tasks(tasks, root)?;

                    // 保留原选中索引，并修正越界情况。
                    select_visible_index(
                        &mut state,
                        tasks,
                        show_completed,
                        selected,
                    );
                }

                continue;
            }

            // -----------------------------------------------------------------
            // r: 手动重新加载
            // -----------------------------------------------------------------

            KeyCode::Char('r') => {
                let selected = state.selected().unwrap_or(0);

                reload_tasks(tasks, root)?;

                select_visible_index(
                    &mut state,
                    tasks,
                    show_completed,
                    selected,
                );

                continue;
            }

            // -----------------------------------------------------------------
            // t: 切换已完成任务
            // -----------------------------------------------------------------

            KeyCode::Char('t') => {
                show_completed = !show_completed;

                select_visible_index(
                    &mut state,
                    tasks,
                    show_completed,
                    0,
                );

                continue;
            }

            _ if row_count == 0 => continue,

            _ => {}
        }

        // ---------------------------------------------------------------------
        // Navigation
        // ---------------------------------------------------------------------

        let current = state.selected().unwrap_or(0);

        let page = terminal
            .size()?
            .height
            .saturating_sub(2)
            .max(1) as usize;

        let next = match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                current
                    .saturating_add(1)
                    .min(row_count - 1)
            }

            KeyCode::Up | KeyCode::Char('k') => {
                current.saturating_sub(1)
            }

            KeyCode::Home => 0,

            KeyCode::End => row_count - 1,

            KeyCode::PageDown => {
                current
                    .saturating_add(page)
                    .min(row_count - 1)
            }

            KeyCode::PageUp => {
                current.saturating_sub(page)
            }

            _ => current,
        };

        state.select(Some(next));
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// External editor
// -----------------------------------------------------------------------------

/// 暂停 TUI，打开任务文件，随后恢复 TUI。
fn open_task_in_editor(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    root: &Path,
    task: &Task,
) -> io::Result<()> {
    disable_raw_mode()?;

    if let Err(error) = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen
    ) {
        let _ = enable_raw_mode();
        return Err(error);
    }

    let _ = terminal.show_cursor();

    let editor_result = run_editor(root, task);

    // 无论编辑器是否成功启动，都尽量恢复 TUI 终端状态。
    let restore_result = (|| {
        enable_raw_mode()?;

        execute!(
            terminal.backend_mut(),
            EnterAlternateScreen
        )?;

        terminal.hide_cursor()?;
        terminal.clear()?;

        Ok::<_, io::Error>(())
    })();

    match (editor_result, restore_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// 根据 VISUAL / EDITOR 启动编辑器，并尽可能定位到任务所在行。
fn run_editor(
    root: &Path,
    task: &Task,
) -> io::Result<()> {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".to_string());

    // 支持类似 "nvim --clean" 这样的简单命令写法。
    // 编辑器命令中的带空格引号参数不做 shell 解析。
    let mut parts = editor.split_whitespace();

    let program = parts.next().unwrap_or("vi");

    let path = root.join(&task.path);

    let mut command = Command::new(program);

    command.args(parts);

    let executable = Path::new(program)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(program);

    match executable {
        // VS Code 系列需要 --goto 指定行，
        // 并等待窗口关闭后再恢复 TUI。
        "code" | "code-insiders" | "codium" => {
            command
                .arg("--wait")
                .arg("--goto")
                .arg(format!(
                    "{}:{}",
                    path.display(),
                    task.line,
                ));
        }

        // 这些编辑器支持 +LINE FILE 的命令行形式。
        "vi" | "vim" | "nvim" | "view" | "nano" | "emacs" => {
            command
                .arg(format!("+{}", task.line))
                .arg(&path);
        }

        // 其他编辑器至少打开对应文件。
        _ => {
            command.arg(&path);
        }
    }

    // status() 继承当前终端的 stdin/stdout/stderr，并等待编辑器退出。
    // 即使编辑器返回非零退出码，也视为它已经退出，
    // 随后由调用方重新加载任务。
    let _ = command.status()?;

    Ok(())
}

// -----------------------------------------------------------------------------
// List rendering
// -----------------------------------------------------------------------------

/// 将任务数据转换为 ratatui 列表项。
///
/// 一个任务对应一个列表项。
///
/// - 已完成任务使用灰色和删除线。
/// - 逾期任务使用红色。
/// - 今天和未来到期的任务使用青色。
/// - 没有截止日期的任务显示 `No due date`。
/// - 文件路径和行号始终显示在任务末尾。
///
/// 已完成任务的隐藏由 `run_app` 控制，
/// 这个函数始终按照传入数据进行渲染。
fn build_list_items(
    tasks: &[&Task],
    today: NaiveDate,
) -> Vec<ListItem<'static>> {
    if tasks.is_empty() {
        return vec![
            ListItem::new("No tasks to display.")
        ];
    }

    tasks
        .iter()
        .map(|task| {
            let checkbox = if task.checked {
                "☑ "
            } else {
                "☐ "
            };

            // 只有存在截止日期时才显示日期标签。
            let (date, date_style) = match task.due {
                Some(due) => {
                    let style = if task.checked {
                        Style::default().fg(Color::DarkGray)
                    } else if due < today {
                        Style::default()
                            .fg(Color::Red)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    };

                    (date_heading(due, today), style)
                }

                None => (
                    "No due date".to_string(),
                    Style::default().fg(Color::DarkGray),
                ),
            };

            // 只有存在截止日期时才需要移除日期标记。
            let description = match task.due {
                Some(due) => {
                    strip_due_annotation(&task.text, due)
                }

                None => task.text.clone(),
            };

            let location = format!(
                "{}:{}",
                task.path.display(),
                task.line,
            );

            let checkbox_style = if task.checked {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Green)
            };

            let description_style = if task.checked {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::CROSSED_OUT)
            } else {
                Style::default()
            };

            let location_style =
                Style::default().fg(Color::DarkGray);

            let line = Line::from(vec![
                Span::styled(
                    checkbox,
                    checkbox_style,
                ),
                Span::styled(
                    format!("{date}  "),
                    date_style,
                ),
                Span::styled(
                    description,
                    description_style,
                ),
                Span::styled(
                    format!("  {location}"),
                    location_style,
                ),
            ]);

            ListItem::new(line)
        })
        .collect()
}

/// 将截止日期转换为适合列表展示的日期标签。
///
/// - 早于今天：显示“逾期 N 天 · YYYY-MM-DD”。
/// - 等于今天：显示“今天 · YYYY-MM-DD”。
/// - 晚于今天 1 天：显示“明天 · YYYY-MM-DD”。
/// - 晚于今天 2 天：显示“后天 · YYYY-MM-DD”。
/// - 更远的日期：只显示 ISO 日期。
fn date_heading(
    date: NaiveDate,
    today: NaiveDate,
) -> String {
    let days = date
        .signed_duration_since(today)
        .num_days();

    match days {
        days if days < 0 => {
            format!("逾期 {} 天 · {date}", -days)
        }

        0 => format!("今天 · {date}"),

        1 => format!("明天 · {date}"),

        2 => format!("后天 · {date}"),

        _ => date.to_string(),
    }
}
