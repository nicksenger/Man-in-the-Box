//! Terminal emulator state and ANSI parsing.

use iced::Color;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

/// A styled span of text.
#[derive(Debug, Clone)]
pub struct Span {
    pub text: String,
    pub fg_color: Color,
    pub bg_color: Option<Color>,
    pub bold: bool,
    pub italic: bool,
}

impl Default for Span {
    fn default() -> Self {
        Self {
            text: String::new(),
            fg_color: Color::WHITE,
            bg_color: None,
            bold: false,
            italic: false,
        }
    }
}

/// A line of terminal output.
#[derive(Debug, Clone, Default)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    fn push_cell(&mut self, cell: &Cell) {
        if cell.wide_continuation {
            return;
        }

        let should_create_new = self.spans.last().is_none_or(|span| {
            span.fg_color != cell.style.fg_color
                || span.bg_color != cell.style.bg_color
                || span.bold != cell.style.bold
                || span.italic != cell.style.italic
        });

        if should_create_new {
            self.spans.push(Span {
                text: String::new(),
                fg_color: cell.style.fg_color,
                bg_color: cell.style.bg_color,
                bold: cell.style.bold,
                italic: cell.style.italic,
            });
        }

        if let Some(span) = self.spans.last_mut() {
            span.text.push_str(cell.text.as_str());
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Style {
    fg_color: Color,
    bg_color: Option<Color>,
    bold: bool,
    italic: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fg_color: Color::WHITE,
            bg_color: None,
            bold: false,
            italic: false,
        }
    }
}

#[derive(Debug, Clone)]
struct Cell {
    text: String,
    style: Style,
    wide_continuation: bool,
}

impl Cell {
    fn blank() -> Self {
        Self::blank_with_style(Style::default())
    }

    fn blank_with_style(style: Style) -> Self {
        Self {
            text: String::from(" "),
            style,
            wide_continuation: false,
        }
    }

    fn continuation(style: Style) -> Self {
        Self {
            text: String::from(" "),
            style,
            wide_continuation: true,
        }
    }
}

#[derive(Clone)]
struct SavedScreen {
    buffer: Vec<Vec<Cell>>,
    cursor_row: usize,
    cursor_col: usize,
    style: Style,
    saved_cursor: (usize, usize),
    scroll_top: usize,
    scroll_bottom: usize,
}

/// Terminal state with cursor tracking.
pub struct Terminal {
    buffer: Vec<Vec<Cell>>,
    cursor_row: usize,
    cursor_col: usize,
    cols: usize,
    rows: usize,
    style: Style,
    parser: Parser,
    saved_cursor: (usize, usize),
    scroll_top: usize,
    scroll_bottom: usize,
    alternate_screen: bool,
    saved_main_screen: Option<SavedScreen>,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);

        let mut buffer = Vec::with_capacity(rows);
        for _ in 0..rows {
            buffer.push(blank_row(cols));
        }

        Self {
            buffer,
            cursor_row: 0,
            cursor_col: 0,
            cols,
            rows,
            style: Style::default(),
            parser: Parser::new(),
            saved_cursor: (0, 0),
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            alternate_screen: false,
            saved_main_screen: None,
        }
    }

    pub fn feed(&mut self, data: &[u8]) {
        let mut performer = TerminalPerformer {
            buffer: &mut self.buffer,
            cursor_row: &mut self.cursor_row,
            cursor_col: &mut self.cursor_col,
            cols: self.cols,
            rows: self.rows,
            style: &mut self.style,
            saved_cursor: &mut self.saved_cursor,
            scroll_top: &mut self.scroll_top,
            scroll_bottom: &mut self.scroll_bottom,
            alternate_screen: &mut self.alternate_screen,
            saved_main_screen: &mut self.saved_main_screen,
        };

        self.parser.advance(&mut performer, data);
    }

    pub fn lines(&self) -> Vec<Line> {
        let mut lines = Vec::with_capacity(self.rows);

        for row in &self.buffer {
            let mut line = Line::default();
            for cell in row {
                line.push_cell(cell);
            }

            if line.spans.is_empty() {
                line.spans.push(Span::default());
            }
            lines.push(line);
        }

        lines
    }
}

struct TerminalPerformer<'a> {
    buffer: &'a mut Vec<Vec<Cell>>,
    cursor_row: &'a mut usize,
    cursor_col: &'a mut usize,
    cols: usize,
    rows: usize,
    style: &'a mut Style,
    saved_cursor: &'a mut (usize, usize),
    scroll_top: &'a mut usize,
    scroll_bottom: &'a mut usize,
    alternate_screen: &'a mut bool,
    saved_main_screen: &'a mut Option<SavedScreen>,
}

impl<'a> TerminalPerformer<'a> {
    fn clamp_cursor(&mut self) {
        *self.cursor_row = (*self.cursor_row).min(self.rows.saturating_sub(1));
        *self.cursor_col = (*self.cursor_col).min(self.cols.saturating_sub(1));
    }

    fn linefeed(&mut self) {
        if *self.cursor_row == *self.scroll_bottom {
            self.scroll_up_region(*self.scroll_top, *self.scroll_bottom, 1);
        } else if *self.cursor_row < self.rows.saturating_sub(1) {
            *self.cursor_row += 1;
        } else {
            self.scroll_up_region(0, self.rows.saturating_sub(1), 1);
        }
    }

    fn reverse_index(&mut self) {
        if *self.cursor_row == *self.scroll_top {
            self.scroll_down_region(*self.scroll_top, *self.scroll_bottom, 1);
        } else if *self.cursor_row > 0 {
            *self.cursor_row -= 1;
        }
    }

    fn put_char(&mut self, c: char) {
        let width = char_width(c);
        if width == 0 {
            self.append_combining_char(c);
            return;
        }

        self.clamp_cursor();

        if *self.cursor_col >= self.cols {
            *self.cursor_col = 0;
            self.linefeed();
        }

        if width == 2 && *self.cursor_col == self.cols.saturating_sub(1) {
            *self.cursor_col = 0;
            self.linefeed();
        }

        let row = *self.cursor_row;
        let col = *self.cursor_col;

        self.clear_overlapped_cell(row, col);

        self.buffer[row][col] = Cell {
            text: c.to_string(),
            style: self.style.clone(),
            wide_continuation: false,
        };

        if width == 2 {
            let right_col = col + 1;
            self.clear_overlapped_cell(row, right_col);
            self.buffer[row][right_col] = Cell::continuation(self.style.clone());
        }

        *self.cursor_col += width;

        if *self.cursor_col >= self.cols {
            *self.cursor_col = 0;
            self.linefeed();
        }
    }

    fn append_combining_char(&mut self, c: char) {
        self.clamp_cursor();
        let row = *self.cursor_row;

        if *self.cursor_col > 0 {
            let mut col = *self.cursor_col - 1;
            if self.buffer[row][col].wide_continuation && col > 0 {
                col -= 1;
            }
            if !self.buffer[row][col].wide_continuation {
                self.buffer[row][col].text.push(c);
                return;
            }
        }

        if row > 0 {
            let prev_row = row - 1;
            for col in (0..self.cols).rev() {
                if !self.buffer[prev_row][col].wide_continuation {
                    self.buffer[prev_row][col].text.push(c);
                    return;
                }
            }
        }
    }

    fn clear_overlapped_cell(&mut self, row: usize, col: usize) {
        if row >= self.rows || col >= self.cols {
            return;
        }

        if self.buffer[row][col].wide_continuation {
            self.buffer[row][col] = Cell::blank();
            if col > 0 && is_wide_lead(&self.buffer[row][col - 1]) {
                self.buffer[row][col - 1] = Cell::blank();
            }
            return;
        }

        if is_wide_lead(&self.buffer[row][col])
            && col + 1 < self.cols
            && self.buffer[row][col + 1].wide_continuation
        {
            self.buffer[row][col + 1] = Cell::blank();
        }

        self.buffer[row][col] = Cell::blank();
    }

    fn sanitize_row(&mut self, row: usize) {
        if row >= self.rows {
            return;
        }

        for col in 0..self.cols {
            if self.buffer[row][col].wide_continuation {
                if col == 0 || !is_wide_lead(&self.buffer[row][col - 1]) {
                    self.buffer[row][col] = Cell::blank();
                }
                continue;
            }

            if is_wide_lead(&self.buffer[row][col]) {
                if col + 1 >= self.cols {
                    self.buffer[row][col] = Cell::blank();
                    continue;
                }

                if !self.buffer[row][col + 1].wide_continuation {
                    let style = self.buffer[row][col].style.clone();
                    self.buffer[row][col + 1] = Cell::continuation(style);
                }
            }
        }
    }

    fn clear_row_range(&mut self, row: usize, start_col: usize, end_col: usize, style: &Style) {
        if row >= self.rows {
            return;
        }

        let start = start_col.min(self.cols);
        let end = end_col.min(self.cols);

        if start >= end {
            return;
        }

        for col in start..end {
            self.buffer[row][col] = Cell::blank_with_style(style.clone());
        }

        self.sanitize_row(row);
    }

    fn clear_screen_all(&mut self, style: &Style) {
        for row in 0..self.rows {
            self.clear_row_range(row, 0, self.cols, style);
        }
    }

    fn clear_to_end_of_screen(&mut self, style: &Style) {
        let row = (*self.cursor_row).min(self.rows.saturating_sub(1));
        let col = (*self.cursor_col).min(self.cols.saturating_sub(1));

        self.clear_row_range(row, col, self.cols, style);

        for r in (row + 1)..self.rows {
            self.clear_row_range(r, 0, self.cols, style);
        }
    }

    fn clear_to_start_of_screen(&mut self, style: &Style) {
        let row = (*self.cursor_row).min(self.rows.saturating_sub(1));
        let col = (*self.cursor_col).min(self.cols.saturating_sub(1));

        for r in 0..row {
            self.clear_row_range(r, 0, self.cols, style);
        }

        self.clear_row_range(row, 0, col.saturating_add(1), style);
    }

    fn insert_blank_chars(&mut self, count: usize) {
        let row = (*self.cursor_row).min(self.rows.saturating_sub(1));
        let col = (*self.cursor_col).min(self.cols.saturating_sub(1));

        if col >= self.cols {
            return;
        }

        let count = count.min(self.cols.saturating_sub(col));
        if count == 0 {
            return;
        }

        for idx in (col..(self.cols - count)).rev() {
            self.buffer[row][idx + count] = self.buffer[row][idx].clone();
        }

        for idx in col..(col + count) {
            self.buffer[row][idx] = Cell::blank_with_style(self.style.clone());
        }

        self.sanitize_row(row);
    }

    fn delete_chars(&mut self, count: usize) {
        let row = (*self.cursor_row).min(self.rows.saturating_sub(1));
        let col = (*self.cursor_col).min(self.cols.saturating_sub(1));

        if col >= self.cols {
            return;
        }

        let count = count.min(self.cols.saturating_sub(col));
        if count == 0 {
            return;
        }

        for idx in col..(self.cols - count) {
            self.buffer[row][idx] = self.buffer[row][idx + count].clone();
        }

        for idx in (self.cols - count)..self.cols {
            self.buffer[row][idx] = Cell::blank_with_style(self.style.clone());
        }

        self.sanitize_row(row);
    }

    fn erase_chars(&mut self, count: usize) {
        let row = (*self.cursor_row).min(self.rows.saturating_sub(1));
        let col = (*self.cursor_col).min(self.cols.saturating_sub(1));

        if col >= self.cols {
            return;
        }

        let end = col.saturating_add(count).min(self.cols);
        for idx in col..end {
            self.buffer[row][idx] = Cell::blank_with_style(self.style.clone());
        }

        self.sanitize_row(row);
    }

    fn insert_lines(&mut self, count: usize) {
        let row = (*self.cursor_row).min(self.rows.saturating_sub(1));
        if row < *self.scroll_top || row > *self.scroll_bottom {
            return;
        }

        let available = self.scroll_bottom.saturating_sub(row).saturating_add(1);
        let count = count.min(available);

        for _ in 0..count {
            for idx in (row + 1..=*self.scroll_bottom).rev() {
                self.buffer[idx] = self.buffer[idx - 1].clone();
            }
            self.buffer[row] = blank_row(self.cols);
        }
    }

    fn delete_lines(&mut self, count: usize) {
        let row = (*self.cursor_row).min(self.rows.saturating_sub(1));
        if row < *self.scroll_top || row > *self.scroll_bottom {
            return;
        }

        let available = self.scroll_bottom.saturating_sub(row).saturating_add(1);
        let count = count.min(available);

        for _ in 0..count {
            for idx in row..*self.scroll_bottom {
                self.buffer[idx] = self.buffer[idx + 1].clone();
            }
            self.buffer[*self.scroll_bottom] = blank_row(self.cols);
        }
    }

    fn scroll_up_region(&mut self, top: usize, bottom: usize, count: usize) {
        if top >= self.rows || bottom >= self.rows || top > bottom {
            return;
        }

        let lines = bottom.saturating_sub(top).saturating_add(1);
        let count = count.min(lines);

        for _ in 0..count {
            for row in top..bottom {
                self.buffer[row] = self.buffer[row + 1].clone();
            }
            self.buffer[bottom] = blank_row(self.cols);
        }
    }

    fn scroll_down_region(&mut self, top: usize, bottom: usize, count: usize) {
        if top >= self.rows || bottom >= self.rows || top > bottom {
            return;
        }

        let lines = bottom.saturating_sub(top).saturating_add(1);
        let count = count.min(lines);

        for _ in 0..count {
            for row in (top + 1..=bottom).rev() {
                self.buffer[row] = self.buffer[row - 1].clone();
            }
            self.buffer[top] = blank_row(self.cols);
        }
    }

    fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        if top < bottom && bottom < self.rows {
            *self.scroll_top = top;
            *self.scroll_bottom = bottom;
        } else {
            *self.scroll_top = 0;
            *self.scroll_bottom = self.rows.saturating_sub(1);
        }

        *self.cursor_row = *self.scroll_top;
        *self.cursor_col = 0;
    }

    fn handle_private_mode(&mut self, mode: u16, set: bool) {
        if mode == 1049 {
            if set {
                self.enter_alternate_screen();
            } else {
                self.leave_alternate_screen();
            }
        }
    }

    fn enter_alternate_screen(&mut self) {
        if *self.alternate_screen {
            return;
        }

        let snapshot = SavedScreen {
            buffer: self.buffer.clone(),
            cursor_row: *self.cursor_row,
            cursor_col: *self.cursor_col,
            style: self.style.clone(),
            saved_cursor: *self.saved_cursor,
            scroll_top: *self.scroll_top,
            scroll_bottom: *self.scroll_bottom,
        };

        *self.saved_main_screen = Some(snapshot);
        *self.alternate_screen = true;

        for row in 0..self.rows {
            self.buffer[row] = blank_row(self.cols);
        }
        *self.cursor_row = 0;
        *self.cursor_col = 0;
        *self.saved_cursor = (0, 0);
        *self.scroll_top = 0;
        *self.scroll_bottom = self.rows.saturating_sub(1);
    }

    fn leave_alternate_screen(&mut self) {
        if !*self.alternate_screen {
            return;
        }

        if let Some(saved) = self.saved_main_screen.take() {
            *self.buffer = saved.buffer;
            *self.cursor_row = saved.cursor_row.min(self.rows.saturating_sub(1));
            *self.cursor_col = saved.cursor_col.min(self.cols.saturating_sub(1));
            *self.style = saved.style;
            *self.saved_cursor = saved.saved_cursor;
            *self.scroll_top = saved.scroll_top.min(self.rows.saturating_sub(1));
            *self.scroll_bottom = saved.scroll_bottom.min(self.rows.saturating_sub(1));

            if *self.scroll_top >= *self.scroll_bottom {
                *self.scroll_top = 0;
                *self.scroll_bottom = self.rows.saturating_sub(1);
            }
        }

        *self.alternate_screen = false;
    }

    fn reset_terminal(&mut self) {
        *self.style = Style::default();
        *self.saved_cursor = (0, 0);
        *self.scroll_top = 0;
        *self.scroll_bottom = self.rows.saturating_sub(1);
        *self.cursor_row = 0;
        *self.cursor_col = 0;

        for row in 0..self.rows {
            self.buffer[row] = blank_row(self.cols);
        }
    }
}

impl<'a> Perform for TerminalPerformer<'a> {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.linefeed(),
            b'\r' => *self.cursor_col = 0,
            b'\t' => {
                let next_tab = ((*self.cursor_col + 8) / 8) * 8;
                *self.cursor_col = next_tab.min(self.cols.saturating_sub(1));
            }
            b'\x08' => {
                if *self.cursor_col > 0 {
                    *self.cursor_col -= 1;
                }
            }
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        match action {
            'H' | 'f' => {
                let row = param_at(params, 0, 1).saturating_sub(1);
                let col = param_at(params, 1, 1).saturating_sub(1);

                *self.cursor_row = row.min(self.rows.saturating_sub(1));
                *self.cursor_col = col.min(self.cols.saturating_sub(1));
            }
            'A' => {
                let n = param_count(params, 1);
                *self.cursor_row = self.cursor_row.saturating_sub(n);
            }
            'B' => {
                let n = param_count(params, 1);
                *self.cursor_row = (*self.cursor_row + n).min(self.rows.saturating_sub(1));
            }
            'C' => {
                let n = param_count(params, 1);
                *self.cursor_col = (*self.cursor_col + n).min(self.cols.saturating_sub(1));
            }
            'D' => {
                let n = param_count(params, 1);
                *self.cursor_col = self.cursor_col.saturating_sub(n);
            }
            'E' => {
                let n = param_count(params, 1);
                *self.cursor_row = (*self.cursor_row + n).min(self.rows.saturating_sub(1));
                *self.cursor_col = 0;
            }
            'F' => {
                let n = param_count(params, 1);
                *self.cursor_row = self.cursor_row.saturating_sub(n);
                *self.cursor_col = 0;
            }
            'G' => {
                let col = param_at(params, 0, 1).saturating_sub(1);
                *self.cursor_col = col.min(self.cols.saturating_sub(1));
            }
            'd' => {
                let row = param_at(params, 0, 1).saturating_sub(1);
                *self.cursor_row = row.min(self.rows.saturating_sub(1));
            }
            'm' => {
                let values: Vec<u16> = params.iter().flat_map(|p| p.iter().copied()).collect();

                if values.is_empty() {
                    *self.style = Style::default();
                    return;
                }

                let mut i = 0;
                while i < values.len() {
                    match values[i] {
                        0 => *self.style = Style::default(),
                        1 => self.style.bold = true,
                        3 => self.style.italic = true,
                        22 => self.style.bold = false,
                        23 => self.style.italic = false,
                        30 => self.style.fg_color = Color::BLACK,
                        31 => self.style.fg_color = Color::from_rgb(0.8, 0.2, 0.2),
                        32 => self.style.fg_color = Color::from_rgb(0.2, 0.8, 0.2),
                        33 => self.style.fg_color = Color::from_rgb(0.8, 0.8, 0.2),
                        34 => self.style.fg_color = Color::from_rgb(0.2, 0.2, 0.8),
                        35 => self.style.fg_color = Color::from_rgb(0.8, 0.2, 0.8),
                        36 => self.style.fg_color = Color::from_rgb(0.2, 0.8, 0.8),
                        37 => self.style.fg_color = Color::WHITE,
                        39 => self.style.fg_color = Color::WHITE,
                        40 => self.style.bg_color = Some(Color::BLACK),
                        41 => self.style.bg_color = Some(Color::from_rgb(0.8, 0.2, 0.2)),
                        42 => self.style.bg_color = Some(Color::from_rgb(0.2, 0.8, 0.2)),
                        43 => self.style.bg_color = Some(Color::from_rgb(0.8, 0.8, 0.2)),
                        44 => self.style.bg_color = Some(Color::from_rgb(0.2, 0.2, 0.8)),
                        45 => self.style.bg_color = Some(Color::from_rgb(0.8, 0.2, 0.8)),
                        46 => self.style.bg_color = Some(Color::from_rgb(0.2, 0.8, 0.8)),
                        47 => self.style.bg_color = Some(Color::WHITE),
                        49 => self.style.bg_color = None,
                        90 => self.style.fg_color = Color::from_rgb(0.5, 0.5, 0.5),
                        91 => self.style.fg_color = Color::from_rgb(1.0, 0.3, 0.3),
                        92 => self.style.fg_color = Color::from_rgb(0.3, 1.0, 0.3),
                        93 => self.style.fg_color = Color::from_rgb(1.0, 1.0, 0.3),
                        94 => self.style.fg_color = Color::from_rgb(0.3, 0.3, 1.0),
                        95 => self.style.fg_color = Color::from_rgb(1.0, 0.3, 1.0),
                        96 => self.style.fg_color = Color::from_rgb(0.3, 1.0, 1.0),
                        97 => self.style.fg_color = Color::WHITE,
                        100 => self.style.bg_color = Some(Color::from_rgb(0.5, 0.5, 0.5)),
                        101 => self.style.bg_color = Some(Color::from_rgb(1.0, 0.3, 0.3)),
                        102 => self.style.bg_color = Some(Color::from_rgb(0.3, 1.0, 0.3)),
                        103 => self.style.bg_color = Some(Color::from_rgb(1.0, 1.0, 0.3)),
                        104 => self.style.bg_color = Some(Color::from_rgb(0.3, 0.3, 1.0)),
                        105 => self.style.bg_color = Some(Color::from_rgb(1.0, 0.3, 1.0)),
                        106 => self.style.bg_color = Some(Color::from_rgb(0.3, 1.0, 1.0)),
                        107 => self.style.bg_color = Some(Color::WHITE),
                        38 => {
                            if i + 2 < values.len() && values[i + 1] == 5 {
                                self.style.fg_color = color_256(values[i + 2]);
                                i += 2;
                            } else if i + 4 < values.len() && values[i + 1] == 2 {
                                self.style.fg_color =
                                    color_true(values[i + 2], values[i + 3], values[i + 4]);
                                i += 4;
                            }
                        }
                        48 => {
                            if i + 2 < values.len() && values[i + 1] == 5 {
                                self.style.bg_color = Some(color_256(values[i + 2]));
                                i += 2;
                            } else if i + 4 < values.len() && values[i + 1] == 2 {
                                self.style.bg_color =
                                    Some(color_true(values[i + 2], values[i + 3], values[i + 4]));
                                i += 4;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
            'J' => {
                let style = self.style.clone();
                let param = param_at(params, 0, 0);
                match param {
                    0 => self.clear_to_end_of_screen(&style),
                    1 => self.clear_to_start_of_screen(&style),
                    2 | 3 => self.clear_screen_all(&style),
                    _ => {}
                }
            }
            'K' => {
                let style = self.style.clone();
                let row = (*self.cursor_row).min(self.rows.saturating_sub(1));
                let col = (*self.cursor_col).min(self.cols.saturating_sub(1));
                let param = param_at(params, 0, 0);
                match param {
                    0 => self.clear_row_range(row, col, self.cols, &style),
                    1 => self.clear_row_range(row, 0, col.saturating_add(1), &style),
                    2 => self.clear_row_range(row, 0, self.cols, &style),
                    _ => {}
                }
            }
            '@' => {
                let count = param_count(params, 1);
                self.insert_blank_chars(count);
            }
            'P' => {
                let count = param_count(params, 1);
                self.delete_chars(count);
            }
            'X' => {
                let count = param_count(params, 1);
                self.erase_chars(count);
            }
            'L' => {
                let count = param_count(params, 1);
                self.insert_lines(count);
            }
            'M' => {
                let count = param_count(params, 1);
                self.delete_lines(count);
            }
            'S' => {
                let count = param_count(params, 1);
                self.scroll_up_region(*self.scroll_top, *self.scroll_bottom, count);
            }
            'T' => {
                let count = param_count(params, 1);
                self.scroll_down_region(*self.scroll_top, *self.scroll_bottom, count);
            }
            'r' => {
                let top = param_at(params, 0, 1).saturating_sub(1);
                let bottom = param_at(params, 1, self.rows as u16).saturating_sub(1);
                self.set_scroll_region(top, bottom);
            }
            's' => *self.saved_cursor = (*self.cursor_row, *self.cursor_col),
            'u' => {
                *self.cursor_row = self.saved_cursor.0.min(self.rows.saturating_sub(1));
                *self.cursor_col = self.saved_cursor.1.min(self.cols.saturating_sub(1));
            }
            'h' | 'l' => {
                if intermediates.contains(&b'?') {
                    let set = action == 'h';
                    for group in params.iter() {
                        for value in group.iter() {
                            self.handle_private_mode(*value, set);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates, byte) {
            ([], b'7') => *self.saved_cursor = (*self.cursor_row, *self.cursor_col),
            ([], b'8') => {
                *self.cursor_row = self.saved_cursor.0.min(self.rows.saturating_sub(1));
                *self.cursor_col = self.saved_cursor.1.min(self.cols.saturating_sub(1));
            }
            ([], b'D') => self.linefeed(),
            ([], b'E') => {
                self.linefeed();
                *self.cursor_col = 0;
            }
            ([], b'M') => self.reverse_index(),
            ([], b'c') => self.reset_terminal(),
            _ => {}
        }
    }
}

fn blank_row(cols: usize) -> Vec<Cell> {
    let mut row = Vec::with_capacity(cols);
    for _ in 0..cols {
        row.push(Cell::blank());
    }
    row
}

fn is_wide_lead(cell: &Cell) -> bool {
    if cell.wide_continuation {
        return false;
    }

    let first = if let Some(ch) = cell.text.chars().next() {
        ch
    } else {
        return false;
    };

    UnicodeWidthChar::width(first).unwrap_or(1) >= 2
}

fn char_width(c: char) -> usize {
    let width = UnicodeWidthChar::width(c).unwrap_or(0);
    if width == 0 { 0 } else { width.min(2) }
}

fn param_at(params: &Params, index: usize, default: u16) -> usize {
    let value = params
        .iter()
        .nth(index)
        .and_then(|param| param.first().copied())
        .unwrap_or(default);
    if value == 0 {
        default as usize
    } else {
        value as usize
    }
}

fn param_count(params: &Params, default: u16) -> usize {
    param_at(params, 0, default)
}

fn color_256(index: u16) -> Color {
    match index {
        0 => Color::BLACK,
        1 => Color::from_rgb(0.8, 0.0, 0.0),
        2 => Color::from_rgb(0.0, 0.8, 0.0),
        3 => Color::from_rgb(0.8, 0.8, 0.0),
        4 => Color::from_rgb(0.0, 0.0, 0.8),
        5 => Color::from_rgb(0.8, 0.0, 0.8),
        6 => Color::from_rgb(0.0, 0.8, 0.8),
        7 => Color::from_rgb(0.75, 0.75, 0.75),
        8 => Color::from_rgb(0.5, 0.5, 0.5),
        9 => Color::from_rgb(1.0, 0.0, 0.0),
        10 => Color::from_rgb(0.0, 1.0, 0.0),
        11 => Color::from_rgb(1.0, 1.0, 0.0),
        12 => Color::from_rgb(0.0, 0.0, 1.0),
        13 => Color::from_rgb(1.0, 0.0, 1.0),
        14 => Color::from_rgb(0.0, 1.0, 1.0),
        15 => Color::WHITE,
        16..=231 => {
            let n = index - 16;
            let r = (n / 36) % 6;
            let g = (n / 6) % 6;
            let b = n % 6;
            Color::from_rgb(
                if r == 0 {
                    0.0
                } else {
                    (r as f32 * 40.0 + 55.0) / 255.0
                },
                if g == 0 {
                    0.0
                } else {
                    (g as f32 * 40.0 + 55.0) / 255.0
                },
                if b == 0 {
                    0.0
                } else {
                    (b as f32 * 40.0 + 55.0) / 255.0
                },
            )
        }
        232..=255 => {
            let gray = ((index - 232) * 10 + 8) as f32 / 255.0;
            Color::from_rgb(gray, gray, gray)
        }
        _ => Color::WHITE,
    }
}

fn color_true(r: u16, g: u16, b: u16) -> Color {
    Color::from_rgb(
        (r.min(255) as f32) / 255.0,
        (g.min(255) as f32) / 255.0,
        (b.min(255) as f32) / 255.0,
    )
}
