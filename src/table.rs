//! 终端表格：按显示宽度对齐（中文/日文等全角字符占 2 列），带 Unicode 边框。
//!
//! 只处理单行文本，不负责 ANSI 颜色；单元格里的换行会被替换成空格。
//! 输出到终端时会自动收缩到终端宽度，避免长名称折行导致边框错位。

use std::io::{self, IsTerminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

pub struct Table {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<String>>,
    /// 显式宽度上限（用于测试/特殊输出）；`None` 时按终端宽度自动收缩。
    max_width: Option<usize>,
}

impl Table {
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|h| h.to_string()).collect(),
            aligns: vec![Align::Left; headers.len()],
            rows: Vec::new(),
            max_width: None,
        }
    }

    /// 强制表格总显示宽度不超过 `width`（超出时截断最宽的列）。
    #[cfg(test)]
    pub fn max_width(mut self, width: usize) -> Self {
        self.max_width = Some(width);
        self
    }

    pub fn align(mut self, col: usize, align: Align) -> Self {
        if let Some(slot) = self.aligns.get_mut(col) {
            *slot = align;
        }
        self
    }

    pub fn row<S: Into<String>>(&mut self, cells: Vec<S>) {
        let mut cells: Vec<String> = cells.into_iter().map(Into::into).collect();
        cells.resize(self.headers.len(), String::new());
        for cell in &mut cells {
            if cell.contains(['\n', '\r', '\t']) {
                *cell = cell.replace(['\n', '\r', '\t'], " ");
            }
        }
        self.rows.push(cells);
    }

    /// 渲染成多行文本（不含结尾换行）。
    pub fn render(&self) -> String {
        let limit = self.max_width.or_else(terminal_width);
        let widths = self.widths(limit);
        let mut out = String::new();
        out.push_str(&self.rule(&widths, '┌', '┬', '┐'));
        out.push('\n');
        out.push_str(&self.render_row(&self.headers, &widths));
        out.push('\n');
        out.push_str(&self.rule(&widths, '├', '┼', '┤'));
        for row in &self.rows {
            out.push('\n');
            out.push_str(&self.render_row(row, &widths));
        }
        out.push('\n');
        out.push_str(&self.rule(&widths, '└', '┴', '┘'));
        out
    }

    fn widths(&self, limit: Option<usize>) -> Vec<usize> {
        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.width()).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.width());
            }
        }
        // 边框：每列 "│ " + 内容 + " │"，另外首尾各 1 列。
        if let Some(limit) = limit.filter(|limit| *limit > 0) {
            let overhead = widths.len() * 3 + 1;
            let available = limit.saturating_sub(overhead);
            while widths.iter().sum::<usize>() > available {
                let Some((i, _)) = widths.iter().enumerate().max_by_key(|(_, width)| **width)
                else {
                    break;
                };
                if widths[i] <= 1 {
                    break;
                }
                widths[i] -= 1;
            }
        }
        widths
    }

    fn render_row(&self, cells: &[String], widths: &[usize]) -> String {
        let mut out = String::from("│");
        for (i, width) in widths.iter().enumerate() {
            out.push(' ');
            out.push_str(&pad(&truncate(&cells[i], *width), *width, self.aligns[i]));
            out.push_str(" │");
        }
        out
    }

    fn rule(&self, widths: &[usize], left: char, mid: char, right: char) -> String {
        let mut out = String::new();
        out.push(left);
        for (i, width) in widths.iter().enumerate() {
            if i > 0 {
                out.push(mid);
            }
            out.push_str(&"─".repeat(width + 2));
        }
        out.push(right);
        out
    }
}

fn pad(s: &str, width: usize, align: Align) -> String {
    let fill = width.saturating_sub(s.width());
    match align {
        Align::Left => format!("{s}{}", " ".repeat(fill)),
        Align::Right => format!("{}{s}", " ".repeat(fill)),
    }
}

/// 截断到指定显示宽度（中文按 2 列计），末尾用 `…` 表示省略。
fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// 当前终端宽度；非终端输出或无法探测时不限制。
fn terminal_width() -> Option<usize> {
    if !io::stdout().is_terminal() {
        return None;
    }
    if let Some(width) = std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|width| *width > 0)
    {
        return Some(width);
    }
    ioctl_width()
}

#[cfg(target_os = "linux")]
fn ioctl_width() -> Option<usize> {
    #[repr(C)]
    struct WinSize {
        rows: u16,
        cols: u16,
        xpixel: u16,
        ypixel: u16,
    }
    // TIOCGWINSZ：读取终端窗口大小。
    const TIOCGWINSZ: std::os::raw::c_ulong = 0x5413;
    unsafe extern "C" {
        fn ioctl(
            fd: std::os::raw::c_int,
            request: std::os::raw::c_ulong,
            ...
        ) -> std::os::raw::c_int;
    }
    let mut size = WinSize {
        rows: 0,
        cols: 0,
        xpixel: 0,
        ypixel: 0,
    };
    // SAFETY：`size` 是可写的合法指针，TIOCGWINSZ 会写入 `WinSize`。
    let rc = unsafe { ioctl(1, TIOCGWINSZ, &mut size) };
    (rc == 0 && size.cols > 0).then_some(size.cols as usize)
}

#[cfg(not(target_os = "linux"))]
fn ioctl_width() -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_cells_stay_aligned() {
        let mut t = Table::new(&["NAME", "SIZE"]).align(1, Align::Right);
        t.row(vec!["庆余年", "1 GiB"]);
        t.row(vec!["abc", "2 GiB"]);

        let out = t.render();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 6);
        let widths: Vec<usize> = lines.iter().map(|l| l.width()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "每行显示宽度应一致: {widths:?}\n{out}"
        );
        assert!(out.contains("│ 庆余年 │ 1 GiB │"), "{out}");
        assert!(out.contains("│ abc    │ 2 GiB │"), "{out}");
    }

    #[test]
    fn wide_tables_shrink_and_truncate_to_max_width() {
        let mut t = Table::new(&["NAME", "DESCRIPTION"]).max_width(30);
        t.row(vec!["abc", "一段很长很长很长很长很长的描述"]);
        t.row(vec!["def", "short"]);

        let out = t.render();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.iter().all(|line| line.width() <= 30), "{out}");
        assert!(
            lines.iter().all(|line| line.width() == lines[0].width()),
            "{out}"
        );
        assert!(out.contains('…'), "{out}");
    }
}
