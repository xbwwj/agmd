use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use chrono::{Datelike, NaiveDate};
use ignore::WalkBuilder;
use pulldown_cmark::{Event as MarkdownEvent, Options, Parser as MarkdownParser, Tag, TagEnd};

// -----------------------------------------------------------------------------
// 数据结构
// -----------------------------------------------------------------------------

/// Markdown 解析过程中尚未结束的列表项。
#[derive(Debug)]
struct PendingItem {
    offset: usize,
    checked: Option<bool>,
    text: String,
}

/// 从 Markdown 中解析出来的任务。
#[derive(Debug)]
struct ParsedTask {
    line: usize,
    checked: bool,
    text: String,
}

/// 完整任务记录。
#[derive(Debug)]
pub struct Task {
    pub due: Option<NaiveDate>,
    pub path: PathBuf,
    pub line: usize,
    pub checked: bool,
    pub text: String,
}

// -----------------------------------------------------------------------------
// 遍历文件
// -----------------------------------------------------------------------------

/// 递归扫描 `root`，读取 Markdown 文件并收集其中的任务。
pub fn collect_tasks(root: &Path) -> Result<Vec<Task>, Box<dyn Error>> {
    let mut tasks = Vec::new();

    let walker = WalkBuilder::new(root)
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .parents(true)
        .follow_links(false)
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }

            let is_dir = entry.file_type().is_some_and(|ty| ty.is_dir());

            !is_dir || !entry.path().join(".git").exists()
        })
        .build();

    for result in walker {
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!("warning: {error}");
                continue;
            }
        };

        let path = entry.path();

        if !entry.file_type().is_some_and(|ty| ty.is_file()) || !is_markdown(path) {
            continue;
        }

        let source = match fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) => {
                eprintln!("warning: cannot read {}: {error}", path.display());
                continue;
            }
        };

        let relative_path = path.strip_prefix(root).unwrap_or(path).to_path_buf();

        for item in parse_task_items(&source) {
            // 截止日期允许不存在。
            // 没有有效日期标记的任务也会正常进入结果集。
            let due = parse_due_date(&item.text);

            tasks.push(Task {
                due,
                path: relative_path.clone(),
                line: item.line,
                checked: item.checked,
                text: item.text,
            });
        }
    }

    Ok(tasks)
}

/// 判断路径是否具有受支持的 Markdown 文件扩展名。
fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

// -----------------------------------------------------------------------------
// Markdown task parsing
// -----------------------------------------------------------------------------

/// 使用 pulldown-cmark 将 Markdown 源码解析为任务列表项。
fn parse_task_items(source: &str) -> Vec<ParsedTask> {
    let parser = MarkdownParser::new_ext(source, Options::ENABLE_TASKLISTS);

    let mut stack: Vec<PendingItem> = Vec::new();
    let mut tasks = Vec::new();

    for (event, range) in parser.into_offset_iter() {
        match event {
            MarkdownEvent::Start(Tag::Item) => {
                stack.push(PendingItem {
                    offset: range.start,
                    checked: None,
                    text: String::new(),
                });
            }

            MarkdownEvent::End(TagEnd::Item) => {
                let Some(item) = stack.pop() else {
                    continue;
                };

                let Some(checked) = item.checked else {
                    continue;
                };

                let text = collapse_whitespace(&item.text);

                if text.is_empty() {
                    continue;
                }

                let line = source[..item.offset]
                    .bytes()
                    .filter(|&b| b == b'\n')
                    .count()
                    + 1;

                tasks.push(ParsedTask {
                    line,
                    checked,
                    text,
                });
            }

            MarkdownEvent::TaskListMarker(checked) => {
                if let Some(item) = stack.last_mut() {
                    item.checked = Some(checked);
                }
            }

            MarkdownEvent::Text(text) | MarkdownEvent::Code(text) => {
                if let Some(item) = stack.last_mut() {
                    item.text.push_str(text.as_ref());
                }
            }

            MarkdownEvent::SoftBreak | MarkdownEvent::HardBreak => {
                if let Some(item) = stack.last_mut() {
                    item.text.push(' ');
                }
            }

            _ => {}
        }
    }

    tasks
}

/// 将连续空白折叠为单个 ASCII 空格，并去掉首尾空白。
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// -----------------------------------------------------------------------------
// Due date parsing
// -----------------------------------------------------------------------------

/// 从任务文本中查找并解析截止日期标记。
///
/// 支持格式：
///
/// - `-> 2026-11-04`：指定年月日。
/// - `-> 2026-11`：指定月份，自动展开为该月份的最后一天。
///
/// 没有找到有效日期时返回 `None`。
fn parse_due_date(text: &str) -> Option<NaiveDate> {
    for (offset, _) in text.match_indices("->") {
        // 跳过 HTML 注释结束符 -->。
        if offset > 0 && text.as_bytes()[offset - 1] == b'-' {
            continue;
        }

        let after = text[offset + 2..].trim_start();

        let token_len = after
            .bytes()
            .take_while(|byte| byte.is_ascii_digit() || *byte == b'-')
            .count();

        if token_len == 0 {
            continue;
        }

        let token = &after[..token_len];

        if after[token_len..]
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            continue;
        }

        if let Some(date) = parse_date_token(token) {
            return Some(date);
        }
    }

    None
}

/// 把单个日期 token 转换为 `NaiveDate`。
///
/// - 长度为 10：按 `%Y-%m-%d` 解析。
/// - 长度为 7：按 `%Y-%m` 解析，返回该月最后一天。
/// - 其他长度：拒绝。
fn parse_date_token(token: &str) -> Option<NaiveDate> {
    match token.len() {
        10 => NaiveDate::parse_from_str(token, "%Y-%m-%d").ok(),
        7 => {
            let first_day = NaiveDate::parse_from_str(&format!("{token}-01"), "%Y-%m-%d").ok()?;

            let (year, month) = if first_day.month() == 12 {
                (first_day.year().checked_add(1)?, 1)
            } else {
                (first_day.year(), first_day.month() + 1)
            };

            NaiveDate::from_ymd_opt(year, month, 1)?.pred_opt()
        }
        _ => None,
    }
}

/// 从任务描述中删除与 `due` 匹配的截止日期标记。
///
/// 只删除一个成功解析且与当前截止日期相同的标记。
pub fn strip_due_annotation(text: &str, due: NaiveDate) -> String {
    for (offset, _) in text.match_indices("->") {
        if offset > 0 && text.as_bytes()[offset - 1] == b'-' {
            continue;
        }

        let after_arrow = &text[offset + 2..];

        let leading = after_arrow.len() - after_arrow.trim_start().len();
        let after = &after_arrow[leading..];

        let len = after
            .bytes()
            .take_while(|b| b.is_ascii_digit() || *b == b'-')
            .count();

        if len == 0 {
            continue;
        }

        let token = &after[..len];

        if after[len..]
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            continue;
        }

        if parse_date_token(token) != Some(due) {
            continue;
        }

        let end = offset + 2 + leading + len;

        let before = text[..offset].trim_end();
        let remaining = text[end..].trim_start();

        return match (before.is_empty(), remaining.is_empty()) {
            (true, _) => remaining.to_string(),
            (_, true) => before.to_string(),
            _ => format!("{before} {remaining}"),
        };
    }

    text.to_string()
}
