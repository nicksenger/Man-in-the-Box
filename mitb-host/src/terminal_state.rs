use vte::{Params, Parser, Perform};

pub(crate) struct PtyTerminalState {
    parser: Parser,
    cursor_row: usize,
    cursor_col: usize,
    rows: usize,
    cols: usize,
    saved_cursor: (usize, usize),
    pending_responses: Vec<Vec<u8>>,
}

impl PtyTerminalState {
    pub(crate) fn new(cols: usize, rows: usize) -> Self {
        Self {
            parser: Parser::new(),
            cursor_row: 0,
            cursor_col: 0,
            rows: rows.max(1),
            cols: cols.max(1),
            saved_cursor: (0, 0),
            pending_responses: Vec::new(),
        }
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut performer = PtyTerminalPerformer {
            cursor_row: &mut self.cursor_row,
            cursor_col: &mut self.cursor_col,
            rows: self.rows,
            cols: self.cols,
            saved_cursor: &mut self.saved_cursor,
            pending_responses: &mut self.pending_responses,
        };
        self.parser.advance(&mut performer, bytes);
        std::mem::take(&mut self.pending_responses)
    }
}

struct PtyTerminalPerformer<'a> {
    cursor_row: &'a mut usize,
    cursor_col: &'a mut usize,
    rows: usize,
    cols: usize,
    saved_cursor: &'a mut (usize, usize),
    pending_responses: &'a mut Vec<Vec<u8>>,
}

impl<'a> PtyTerminalPerformer<'a> {
    fn linefeed(&mut self) {
        *self.cursor_row = (*self.cursor_row + 1).min(self.rows.saturating_sub(1));
    }

    fn put_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        *self.cursor_col += if char_is_wide(c) { 2 } else { 1 };
        if *self.cursor_col >= self.cols {
            *self.cursor_col = 0;
            self.linefeed();
        }
    }

    fn enqueue_cursor_report(&mut self) {
        let row = self.cursor_row.saturating_add(1);
        let col = self.cursor_col.saturating_add(1);
        self.pending_responses
            .push(format!("\x1b[{row};{col}R").into_bytes());
    }
}

impl Perform for PtyTerminalPerformer<'_> {
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
                *self.cursor_col = self.cursor_col.saturating_sub(1);
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
            'H' | 'f' => {
                let row = param_at(params, 0, 1).saturating_sub(1);
                let col = param_at(params, 1, 1).saturating_sub(1);
                *self.cursor_row = row.min(self.rows.saturating_sub(1));
                *self.cursor_col = col.min(self.cols.saturating_sub(1));
            }
            'd' => {
                let row = param_at(params, 0, 1).saturating_sub(1);
                *self.cursor_row = row.min(self.rows.saturating_sub(1));
            }
            'n' => {
                if intermediates.is_empty() && param_at(params, 0, 0) == 6 {
                    self.enqueue_cursor_report();
                }
            }
            's' => *self.saved_cursor = (*self.cursor_row, *self.cursor_col),
            'u' => {
                *self.cursor_row = self.saved_cursor.0.min(self.rows.saturating_sub(1));
                *self.cursor_col = self.saved_cursor.1.min(self.cols.saturating_sub(1));
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
            _ => {}
        }
    }
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

fn char_is_wide(c: char) -> bool {
    !c.is_ascii() && c.len_utf8() > 1
}
