//! 终端表格：按显示宽度对齐（中文/日文等全角字符占 2 列），带 Unicode 边框。
//!
//! 只处理单行文本，不负责 ANSI 颜色；单元格里的换行会被替换成空格。

use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

pub struct Table {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|h| h.to_string()).collect(),
            aligns: vec![Align::Left; headers.len()],
            rows: Vec::new(),
        }
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
        let widths = self.widths();
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

    fn widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.width()).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.width());
            }
        }
        widths
    }

    fn render_row(&self, cells: &[String], widths: &[usize]) -> String {
        let mut out = String::from("│");
        for (i, width) in widths.iter().enumerate() {
            out.push(' ');
            out.push_str(&pad(&cells[i], *width, self.aligns[i]));
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
}
