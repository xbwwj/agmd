use std::error::Error;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use ignore::WalkBuilder;
use ratatui::{
    Frame, Terminal,
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use regex::Regex;

// 自动刷新间隔
const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

// ---------- 数据模型 ----------

#[derive(Debug, Clone)]
struct Item {
    path: PathBuf,
    line: usize,
    kind: ItemKind,
}

#[derive(Debug, Clone)]
enum ItemKind {
    Heading(u8, String),
    Task {
        done: bool,
        indent: usize,
        body: String,
    },
}

// ---------- 扫描 ----------

fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("md") | Some("markdown") | Some("mdown") | Some("mkd")
    )
}

fn is_readme_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .map_or(false, |n| n.eq_ignore_ascii_case("README.md"))
}

fn visual_indent(s: &str) -> usize {
    s.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

fn extract(path: &Path, content: &str, heading_re: &Regex, task_re: &Regex) -> Vec<Item> {
    let mut out = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let line_no = idx + 1;

        if let Some(c) = heading_re.captures(line) {
            let raw_level = c[1].len() as u8;
            // 非 README 文件的 heading 整体下移一级，让 README 处于顶层
            let level = if is_readme_path(path) {
                raw_level
            } else {
                (raw_level + 1).min(6)
            };
            let text = c[2].trim().to_string();
            out.push(Item {
                path: path.to_path_buf(),
                line: line_no,
                kind: ItemKind::Heading(level, text),
            });
        } else if let Some(c) = task_re.captures(line) {
            let indent = visual_indent(&c[1]);
            let done = matches!(&c[2], "x" | "X");
            let body = c[3].trim_end().to_string();
            out.push(Item {
                path: path.to_path_buf(),
                line: line_no,
                kind: ItemKind::Task { done, indent, body },
            });
        }
    }
    out
}

fn readme_priority(path: &Path) -> u8 {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");

    if name.eq_ignore_ascii_case("README.md") {
        // 根目录下的 README 排最前
        if path.parent().map_or(true, |p| p.as_os_str().is_empty()) {
            0
        } else {
            1
        }
    } else {
        2
    }
}

fn scan_markdown() -> Result<Vec<Item>, String> {
    let heading_re =
        Regex::new(r"^(#{1,6})[ \t]+(.*?)[ \t]*#*[ \t]*$").map_err(|e| e.to_string())?;
    let task_re = Regex::new(r"^([ \t]*)(?:[-*+]|\d+[.)])[ \t]+\[([ xX])\][ \t]+(.*)$")
        .map_err(|e| e.to_string())?;

    let mut items = Vec::new();

    for entry in WalkBuilder::new(".").standard_filters(true).build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map_or(false, |ft| ft.is_file()) {
            continue;
        }

        let raw = entry.path();
        if !is_markdown(raw) {
            continue;
        }

        let display_path = raw.strip_prefix(".").unwrap_or(raw).to_path_buf();

        let content = match fs::read_to_string(raw) {
            Ok(c) => c,
            Err(_) => continue,
        };

        items.extend(extract(&display_path, &content, &heading_re, &task_re));
    }

    items.sort_by(|a, b| {
        readme_priority(&a.path)
            .cmp(&readme_priority(&b.path))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });

    Ok(items)
}

// ---------- 应用状态 ----------

struct App {
    items: Vec<Item>,
    state: ListState,
    status: Option<String>,
    hide_done: bool,

    refresh_rx: Option<Receiver<Result<Vec<Item>, String>>>,
    pending_anchor: Option<(PathBuf, usize)>,

    /// 自动刷新开关
    auto_refresh: bool,
    /// 最近一次发起刷新的时刻（用于自动刷新的周期判断）
    last_refresh_at: Instant,
}

impl App {
    fn new(items: Vec<Item>) -> Self {
        let mut state = ListState::default();
        if !items.is_empty() {
            state.select(Some(0));
        }
        Self {
            items,
            state,
            status: None,
            hide_done: false,
            refresh_rx: None,
            pending_anchor: None,
            auto_refresh: false,
            last_refresh_at: Instant::now(),
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        if !self.hide_done {
            return (0..self.items.len()).collect();
        }

        // 计算每个 Heading 的管辖范围结束索引（不包含）
        let mut heading_ranges: Vec<(usize, usize)> = Vec::new();
        let mut stack: Vec<(usize, u8)> = Vec::new();

        for (i, item) in self.items.iter().enumerate() {
            if let ItemKind::Heading(level, _) = &item.kind {
                while let Some(&(h_idx, h_level)) = stack.last() {
                    if h_level >= *level {
                        stack.pop();
                        heading_ranges.push((h_idx, i));
                    } else {
                        break;
                    }
                }
                stack.push((i, *level));
            }
        }
        for (h_idx, _) in stack {
            heading_ranges.push((h_idx, self.items.len()));
        }

        // 判断每个 Heading 是否应该显示
        // 规则：如果管辖范围内有任务，且所有任务都已完成，则隐藏该 Heading
        let mut heading_visible: Vec<bool> = vec![true; self.items.len()];
        for &(h_idx, end) in &heading_ranges {
            let mut has_task = false;
            let mut has_undone = false;
            for j in h_idx + 1..end {
                if let ItemKind::Task { done, .. } = &self.items[j].kind {
                    has_task = true;
                    if !*done {
                        has_undone = true;
                        break;
                    }
                }
            }
            heading_visible[h_idx] = !has_task || has_undone;
        }

        // 过滤出可见项
        let mut result = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            let keep = match &item.kind {
                ItemKind::Heading(..) => heading_visible[i],
                ItemKind::Task { done, .. } => !*done,
            };
            if keep {
                result.push(i);
            }
        }
        result
    }

    fn selected(&self) -> Option<&Item> {
        self.state.selected().and_then(|i| self.items.get(i))
    }

    fn next(&mut self) {
        let vis = self.visible_indices();
        if vis.is_empty() {
            self.state.select(None);
            return;
        }
        let cur = self.state.selected();
        let pos = cur
            .and_then(|c| vis.iter().position(|&i| i == c))
            .map(|p| (p + 1) % vis.len())
            .unwrap_or(0);
        self.state.select(Some(vis[pos]));
    }

    fn prev(&mut self) {
        let vis = self.visible_indices();
        if vis.is_empty() {
            self.state.select(None);
            return;
        }
        let cur = self.state.selected();
        let pos = cur
            .and_then(|c| vis.iter().position(|&i| i == c))
            .map(|p| if p == 0 { vis.len() - 1 } else { p - 1 })
            .unwrap_or(vis.len() - 1);
        self.state.select(Some(vis[pos]));
    }

    fn first(&mut self) {
        let vis = self.visible_indices();
        self.state.select(vis.first().copied());
    }

    fn last(&mut self) {
        let vis = self.visible_indices();
        self.state.select(vis.last().copied());
    }

    fn step(&mut self, delta: isize) {
        let vis = self.visible_indices();
        if vis.is_empty() {
            self.state.select(None);
            return;
        }
        let cur = self.state.selected();
        let pos = cur
            .and_then(|c| vis.iter().position(|&i| i == c))
            .unwrap_or(0) as isize;
        let new_pos = (pos + delta).clamp(0, vis.len() as isize - 1) as usize;
        self.state.select(Some(vis[new_pos]));
    }

    fn toggle_hide_done(&mut self) {
        self.hide_done = !self.hide_done;

        let vis = self.visible_indices();
        let still_visible = self
            .state
            .selected()
            .map(|c| vis.contains(&c))
            .unwrap_or(false);

        if !still_visible {
            self.state.select(vis.first().copied());
        }
    }

    /// 切换自动刷新开关。开启时重置计时，避免刚打开就立刻触发。
    fn toggle_auto_refresh(&mut self) {
        self.auto_refresh = !self.auto_refresh;
        self.last_refresh_at = Instant::now();
    }

    fn is_refreshing(&self) -> bool {
        self.refresh_rx.is_some()
    }

    /// 若开启自动刷新且间隔已到且当前没在刷新，就发起一次后台刷新
    fn maybe_auto_refresh(&mut self) {
        if !self.auto_refresh || self.is_refreshing() {
            return;
        }
        if self.last_refresh_at.elapsed() >= AUTO_REFRESH_INTERVAL {
            self.start_refresh();
        }
    }

    fn start_refresh(&mut self) {
        if self.refresh_rx.is_some() {
            return;
        }
        // 立刻重置计时，避免长扫描期间反复触发
        self.last_refresh_at = Instant::now();

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(scan_markdown());
        });
        self.refresh_rx = Some(rx);
        self.pending_anchor = self.selected().map(|i| (i.path.clone(), i.line));
    }

    fn poll_refresh(&mut self) {
        let rx = match &self.refresh_rx {
            Some(rx) => rx,
            None => return,
        };

        match rx.try_recv() {
            Ok(Ok(new_items)) => {
                let old_idx = self.state.selected().unwrap_or(0);
                self.items = new_items;

                let restored = self.pending_anchor.as_ref().and_then(|(p, l)| {
                    self.items
                        .iter()
                        .position(|it| &it.path == p && it.line == *l)
                });
                let idx = restored.or_else(|| {
                    if self.items.is_empty() {
                        None
                    } else {
                        Some(old_idx.min(self.items.len() - 1))
                    }
                });
                self.state.select(idx);

                let vis = self.visible_indices();
                if let Some(c) = self.state.selected() {
                    if !vis.contains(&c) {
                        self.state.select(vis.first().copied());
                    }
                }

                self.status = None;
                self.refresh_rx = None;
                self.pending_anchor = None;
            }
            Ok(Err(e)) => {
                self.status = Some(format!("刷新失败: {e}"));
                self.refresh_rx = None;
                self.pending_anchor = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.status = Some("刷新线程意外退出".to_string());
                self.refresh_rx = None;
                self.pending_anchor = None;
            }
        }
    }
}

// ---------- 渲染 ----------

fn heading_color(level: u8) -> Color {
    match level {
        1 => Color::LightRed,
        2 => Color::LightYellow,
        3 => Color::LightGreen,
        4 => Color::LightCyan,
        5 => Color::LightBlue,
        6 => Color::LightMagenta,
        _ => Color::White,
    }
}

fn item_to_line(item: &Item) -> Line<'static> {
    match &item.kind {
        ItemKind::Heading(level, text) => {
            let color = heading_color(*level);
            let hashes = "#".repeat(*level as usize);

            let mut spans = vec![
                Span::styled(
                    hashes,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    text.clone(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ];

            if *level == 1 {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("{}:{}", item.path.display(), item.line),
                    Style::default().fg(Color::DarkGray),
                ));
            }

            Line::from(spans)
        }
        ItemKind::Task { done, indent, body } => {
            let pad = " ".repeat((*indent).min(24));
            let (check, check_style, body_style) = if *done {
                (
                    "[x]",
                    Style::default().fg(Color::Green),
                    Style::default().dim(),
                )
            } else {
                (
                    "[ ]",
                    Style::default().fg(Color::Red),
                    Style::default().fg(Color::White),
                )
            };
            Line::from(vec![
                Span::raw(pad),
                Span::styled(check, check_style),
                Span::raw(" "),
                Span::styled(body.clone(), body_style),
            ])
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(f, app, chunks[0]);
    render_list(f, app, chunks[1]);
    render_footer(f, app, chunks[2]);
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let headings = app
        .items
        .iter()
        .filter(|i| matches!(i.kind, ItemKind::Heading(..)))
        .count();

    let mut tasks = 0usize;
    let mut done = 0usize;
    for it in &app.items {
        if let ItemKind::Task { done: d, .. } = it.kind {
            tasks += 1;
            if d {
                done += 1;
            }
        }
    }

    let pct = if tasks == 0 { 0 } else { (done * 100) / tasks };

    let line = Line::from(vec![
        Span::styled(
            " md-scan ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("Total {}", app.items.len()),
            Style::default().fg(Color::White),
        ),
        Span::raw("  "),
        Span::styled(
            format!("Heading {}", headings),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("  "),
        Span::styled(
            format!("Task {done}/{tasks} ({pct}%)"),
            Style::default().fg(Color::Green),
        ),
    ]);

    f.render_widget(Paragraph::new(line), area);
}

fn render_list(f: &mut Frame, app: &mut App, area: Rect) {
    let vis = app.visible_indices();
    let items: Vec<ListItem> = vis
        .iter()
        .map(|&i| ListItem::new(item_to_line(&app.items[i])))
        .collect();

    let selected_pos = app
        .state
        .selected()
        .and_then(|c| vis.iter().position(|&i| i == c));

    let mut render_state = ListState::default();
    render_state.select(selected_pos);
    *render_state.offset_mut() = app.state.offset();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(36, 44, 60))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    f.render_stateful_widget(list, area, &mut render_state);

    *app.state.offset_mut() = render_state.offset();
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let refreshing = app.is_refreshing();

    // 统一的黄色高亮样式
    let on_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let off_style = Style::default().fg(Color::DarkGray);

    let keys = |hide_done: bool, refreshing: bool, auto_refresh: bool| -> Vec<Span<'static>> {
        let d_style = if hide_done { on_style } else { off_style };
        let r_style = if refreshing { on_style } else { off_style };
        let a_style = if auto_refresh { on_style } else { off_style };

        vec![
            Span::styled("j/k · PgUp/PgDn · g/G · Enter 编辑 · ", off_style),
            Span::styled("d 过滤", d_style),
            Span::styled(" · ", off_style),
            Span::styled("r 刷新", r_style),
            Span::styled(" · ", off_style),
            Span::styled("a 自动刷新", a_style),
            Span::styled(" · q 退出", off_style),
        ]
    };

    if let Some(msg) = &app.status {
        let mut spans = vec![
            Span::raw(" "),
            Span::styled(msg.clone(), Style::default().fg(Color::LightYellow)),
            Span::raw("  "),
        ];
        spans.extend(keys(app.hide_done, refreshing, app.auto_refresh));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let line = if let Some(item) = app.selected() {
        let mut spans = vec![
            Span::styled(
                format!(" {}:{} ", item.path.display(), item.line),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("  "),
        ];
        spans.extend(keys(app.hide_done, refreshing, app.auto_refresh));
        Line::from(spans)
    } else {
        Line::from(Span::styled(
            " 没有找到任何标题或任务",
            Style::default().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

/// 暂停 TUI，用 nvim 打开文件并跳转到指定行，退出后恢复 TUI
fn open_in_nvim<B>(terminal: &mut Terminal<B>, path: &Path, line: usize) -> io::Result<()>
where
    B: Backend + Write,
    io::Error: From<B::Error>,
{
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    let status = Command::new("nvim")
        .arg(format!("+{line}"))
        .arg(path)
        .status();

    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        Clear(ClearType::All)
    )?;
    terminal.clear()?;

    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            eprintln!("nvim exited with status: {s}");
            Ok(())
        }
        Err(e) => Err(io::Error::new(
            io::ErrorKind::Other,
            format!("failed to launch nvim: {e}"),
        )),
    }
}

// ---------- 主循环 ----------

fn run_app<B>(terminal: &mut Terminal<B>, mut app: App) -> io::Result<()>
where
    B: Backend + Write,
    io::Error: From<B::Error>,
{
    loop {
        // 处理后台结果，并检查是否需要自动刷新
        app.poll_refresh();
        app.maybe_auto_refresh();

        terminal.draw(|f| ui(f, &mut app))?;

        // 50ms 超时：自动刷新粒度足够细；空闲时也不会太费
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') => app.next(),
                    KeyCode::Up | KeyCode::Char('k') => app.prev(),
                    KeyCode::Home | KeyCode::Char('g') => app.first(),
                    KeyCode::End | KeyCode::Char('G') => app.last(),
                    KeyCode::PageDown => app.step(15),
                    KeyCode::PageUp => app.step(-15),
                    KeyCode::Char('d') => app.toggle_hide_done(),
                    KeyCode::Char('r') => app.start_refresh(),
                    KeyCode::Char('a') => app.toggle_auto_refresh(),
                    KeyCode::Enter => {
                        if let Some(item) = app.selected() {
                            let path = item.path.clone();
                            let line = item.line;

                            match open_in_nvim(terminal, &path, line) {
                                Ok(()) => app.start_refresh(),
                                Err(e) => {
                                    app.status = Some(format!("nvim 启动失败: {e}"));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let items = scan_markdown().map_err(|e| -> Box<dyn Error> { e.into() })?;

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let app = App::new(items);
    let res = run_app(&mut terminal, app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(e) = res {
        eprintln!("运行错误: {e}");
    }
    Ok(())
}
