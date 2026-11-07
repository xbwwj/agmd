use std::error::Error;

use chrono::Local;

use crate::tui::run_tui;

mod parse;
mod tui;

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::current_dir()?;
    let today = Local::now().date_naive();

    // 运行 TUI。任务列表由 TUI 持有，并可通过 r 或编辑器退出重新加载。
    run_tui(&root, today)?;

    Ok(())
}
