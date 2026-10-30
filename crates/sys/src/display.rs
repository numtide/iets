//! The bottom of the terminal as a live region under the scrolling output.
//!
//! Output lines are written at absolute rows inside a scroll margin that
//! ends above the region, so the region is never touched by them. The size is
//! polled on every frame. While it changes nothing is erased. Once it holds
//! still the screen is rebuilt from the lines kept in `history`, because
//! terminals disagree on how they reflow what is already drawn. Every frame is
//! one string wrapped in synchronized-update markers (tty_renderer.py:Display).

use std::collections::VecDeque;
use std::fmt::Write;

use crate::tty;

const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const SYNC_START: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";
const RESET: &str = "\x1b[0m";
/// Frames a smaller region must last before the region gives up its rows.
const SHRINK_AFTER: usize = 30;
/// Lines kept to rebuild the screen from.
const HISTORY: usize = 1000;

/// Columns and rows.
pub type Size = (usize, usize);

/// What a display asks of the terminal.
pub trait Term: Send {
    /// `None` when there is no terminal to draw on.
    fn size(&self) -> Option<Size>;
    /// Row and column of the cursor, counted from 1.
    fn cursor(&self) -> Option<(usize, usize)>;
    /// Where the region starts, for whatever must take it down on a signal.
    fn guard(&self, top: usize);
}

/// stderr's terminal.
pub struct Tty;

impl Term for Tty {
    fn size(&self) -> Option<Size> {
        tty::size()
    }

    fn cursor(&self) -> Option<(usize, usize)> {
        tty::cursor()
    }

    fn guard(&self, top: usize) {
        tty::guard_region(top);
    }
}

pub struct Display {
    /// Row, counted from 1, that the next output line goes to.
    row: usize,
    /// First row of the region as last drawn.
    top: usize,
    /// Bottom row of the scroll margin, 0 for none.
    margin: usize,
    size: Size,
    resizing: bool,
    /// Rows the region keeps, so it does not twitch with every build that
    /// starts or ends.
    height: usize,
    /// Frames in a row that needed fewer rows than `height`.
    calm: usize,
    /// The most rows those frames needed.
    peak: usize,
    history: VecDeque<String>,
}

impl Display {
    /// A display whose first output line goes to `origin`, the row the cursor
    /// is on.
    pub fn new(origin: usize) -> Display {
        Display {
            row: origin,
            top: origin,
            margin: 0,
            size: (0, 0),
            resizing: false,
            height: 0,
            calm: 0,
            peak: 0,
            history: VecDeque::new(),
        }
    }

    /// The first row of the region, for a signal handler that must clear it.
    pub fn top(&self) -> usize {
        self.top
    }

    /// One update: `output` lines scroll up above the region, which is
    /// redrawn as `region`.
    pub fn frame(&mut self, size: Size, output: &[String], region: &[String]) -> String {
        let (cols, rows) = size;
        // one row stays for output
        let cap = rows.saturating_sub(2).max(1);
        let region = &region[..region.len().min(cap)];
        let height = self.settle(region.len()).min(cap);
        let limit = rows - height;
        let mut buf = String::from(HIDE_CURSOR);
        self.history.extend(output.iter().cloned());
        while self.history.len() > HISTORY {
            self.history.pop_front();
        }
        let mut output = output;
        if size != self.size {
            // absolute rows are unreliable now: release the margin, erase
            // nothing and skip the region until the size settles
            buf += "\x1b[r";
            self.margin = 0;
            self.resizing = self.size.1 != 0;
            self.size = size;
            self.row = self.row.min(rows);
        } else if self.resizing {
            self.resizing = false;
            buf += "\x1b[2J\x1b[1;1H";
            self.row = self.rebuild(&mut buf, limit);
            output = &[];
        }
        if !self.resizing && self.row > limit + 1 {
            // the region grew over output, which scrolls up out of its way
            self.scroll(&mut buf, self.row - 1, self.row - limit - 1);
            self.row = limit + 1;
        }
        for line in output {
            let n = rows_of(line, cols);
            if self.row + n - 1 <= limit {
                put(&mut buf, self.row, line, cols);
                self.row += n;
            } else {
                // scroll up as far as this line needs, then put it at the bottom
                self.scroll(&mut buf, limit, self.row + n - 1 - limit);
                put(&mut buf, (limit + 1).saturating_sub(n).max(1), line, cols);
                self.row = limit + 1;
            }
        }
        self.top = self.row.min(limit + 1);
        if !self.resizing {
            for i in 0..height {
                let line = region.get(i).map_or("", String::as_str);
                // one column short, so the cursor never waits to wrap
                let _ = write!(
                    buf,
                    "\x1b[{};1H{}{RESET}\x1b[K",
                    self.top + i,
                    fit(line, cols.saturating_sub(1))
                );
            }
            buf += "\x1b[J";
        }
        let _ = write!(buf, "\x1b[{};1H", self.top);
        format!("{SYNC_START}{buf}{SYNC_END}")
    }

    /// The rows to keep for a region that needs `need`. It grows at once and
    /// shrinks once a smaller size has lasted.
    fn settle(&mut self, need: usize) -> usize {
        if need >= self.height || need == 0 {
            (self.height, self.calm, self.peak) = (need, 0, 0);
        } else {
            self.calm += 1;
            self.peak = self.peak.max(need);
            if self.calm >= SHRINK_AFTER {
                (self.height, self.calm, self.peak) = (self.peak, 0, 0);
            }
        }
        self.height
    }

    /// Takes the region down and lets output scroll the whole screen again.
    pub fn close(&self) -> String {
        format!(
            "{SYNC_START}\x1b[r\x1b[{};1H\x1b[J{SHOW_CURSOR}{SYNC_END}",
            self.top
        )
    }

    /// Writes the newest lines that fit in `budget` rows from the top.
    /// Returns the row after them.
    fn rebuild(&self, buf: &mut String, budget: usize) -> usize {
        let cols = self.size.0;
        let (mut used, mut take) = (0, 0);
        for line in self.history.iter().rev() {
            let n = rows_of(line, cols);
            if used + n > budget {
                break;
            }
            used += n;
            take += 1;
        }
        let mut row = 1;
        for line in self.history.iter().skip(self.history.len() - take) {
            put(buf, row, line, cols);
            row += rows_of(line, cols);
        }
        row
    }

    /// Scrolls rows 1 to `bottom` up by `n`, into the scrollback.
    fn scroll(&mut self, buf: &mut String, bottom: usize, n: usize) {
        if self.margin != bottom {
            self.margin = bottom;
            let _ = write!(buf, "\x1b[1;{bottom}r");
        }
        let _ = write!(buf, "\x1b[{bottom};1H{}", "\n".repeat(n.min(bottom)));
    }
}

/// `line` at `row`, over whatever was there.
fn put(buf: &mut String, row: usize, line: &str, cols: usize) {
    let _ = write!(buf, "\x1b[{row};1H{line}{RESET}");
    // a line that ends at the last column leaves the cursor waiting to wrap,
    // and erasing from there would take that column's character along
    let width = visible_width(line);
    if width == 0 || !width.is_multiple_of(cols) {
        buf.push_str("\x1b[K");
    }
}

/// How many rows `line` takes at `cols` columns.
fn rows_of(line: &str, cols: usize) -> usize {
    visible_width(line).div_ceil(cols).max(1)
}

/// Columns `s` takes, leaving its escape codes out.
pub fn visible_width(s: &str) -> usize {
    let mut n = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            n += 1;
        } else if chars.next() == Some('[') {
            for t in chars.by_ref() {
                if ('@'..='~').contains(&t) {
                    break;
                }
            }
        }
    }
    n
}

/// `line` cut to `width` visible columns, escape codes kept whole. A cut
/// ends with a reset, so a dropped code cannot leave its colour on.
pub fn fit(line: &str, width: usize) -> String {
    let mut out = String::with_capacity(line.len());
    let (mut seen, mut cut) = (0, false);
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            if let Some(open) = chars.next() {
                out.push(open);
                if open == '[' {
                    for n in chars.by_ref() {
                        out.push(n);
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
            }
        } else if seen < width {
            out.push(c);
            seen += 1;
        } else {
            cut = true;
        }
    }
    if cut {
        out.push_str(RESET);
    }
    out
}

#[cfg(any(test, feature = "testkit"))]
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod tests {
    use super::*;

    const W: usize = 60;
    const H: usize = 12;

    /// A terminal that gets what a display wrote: cursor addressing, erase,
    /// scroll margins and autowrap, no reflow on resize. Like xterm, an erase
    /// at the last column takes that column along.
    pub struct Emu {
        grid: Vec<Vec<char>>,
        row: usize,
        col: usize,
        /// The last column was written and the cursor waits to wrap.
        wrap: bool,
        /// First and last row of the scroll margin.
        margin: (usize, usize),
    }

    impl Emu {
        pub fn new(rows: usize, cols: usize) -> Emu {
            Emu {
                grid: vec![vec![' '; cols]; rows],
                row: 0,
                col: 0,
                wrap: false,
                margin: (0, rows - 1),
            }
        }

        fn cols(&self) -> usize {
            self.grid[0].len()
        }

        fn line_feed(&mut self) {
            if self.row == self.margin.1 {
                self.grid.remove(self.margin.0);
                self.grid.insert(self.margin.1, vec![' '; self.cols()]);
            } else if self.row + 1 < self.grid.len() {
                self.row += 1;
            }
        }

        pub fn feed(&mut self, s: &str) {
            let mut chars = s.chars();
            while let Some(c) = chars.next() {
                match c {
                    '\x1b' => {
                        assert_eq!(chars.next(), Some('['), "only CSI is emulated");
                        let mut params = String::new();
                        let end = chars.by_ref().find(|t| {
                            let last = ('@'..='~').contains(t);
                            if !last {
                                params.push(*t);
                            }
                            last
                        });
                        self.csi(&params, end.expect("an unfinished escape code"));
                    }
                    '\n' => self.line_feed(),
                    '\r' => {
                        self.col = 0;
                        self.wrap = false;
                    }
                    c => {
                        if self.wrap {
                            self.col = 0;
                            self.wrap = false;
                            self.line_feed();
                        }
                        self.grid[self.row][self.col] = c;
                        match self.col + 1 == self.cols() {
                            true => self.wrap = true,
                            false => self.col += 1,
                        }
                    }
                }
            }
        }

        fn csi(&mut self, params: &str, end: char) {
            let n: Vec<usize> = params.split(';').filter_map(|p| p.parse().ok()).collect();
            let at = |i: usize| n.get(i).copied().unwrap_or(0);
            let (rows, cols) = (self.grid.len(), self.cols());
            match end {
                'H' => {
                    self.row = at(0).max(1).min(rows) - 1;
                    self.col = at(1).max(1).min(cols) - 1;
                    self.wrap = false;
                }
                'K' => self.grid[self.row][self.col..].fill(' '),
                'J' if at(0) == 2 => self.grid.iter_mut().for_each(|r| r.fill(' ')),
                'J' => {
                    self.grid[self.row][self.col..].fill(' ');
                    self.grid[self.row + 1..]
                        .iter_mut()
                        .for_each(|r| r.fill(' '));
                }
                'r' => {
                    let (top, bottom) = (at(0).max(1), if at(1) == 0 { rows } else { at(1) });
                    self.margin = (top - 1, bottom.min(rows) - 1);
                    self.row = 0;
                    self.col = 0;
                    self.wrap = false;
                }
                // colours and modes change nothing drawn
                'm' | 'h' | 'l' => {}
                other => panic!("unemulated CSI {params}{other}"),
            }
        }

        pub fn resize(&mut self, rows: usize, cols: usize) {
            self.grid.resize(rows, vec![' '; cols]);
            self.grid.iter_mut().for_each(|r| r.resize(cols, ' '));
            self.margin = (0, rows - 1);
            self.row = self.row.min(rows - 1);
            self.col = self.col.min(cols - 1);
            self.wrap = false;
        }

        pub fn rows(&self) -> Vec<String> {
            let line = |r: &Vec<char>| r.iter().collect::<String>().trim_end().to_string();
            self.grid.iter().map(line).collect()
        }

        pub fn cursor(&self) -> (usize, usize) {
            (self.row, self.col)
        }
    }

    fn lines(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    /// Feeds one frame to the terminal.
    fn show(d: &mut Display, e: &mut Emu, size: Size, output: &[&str], region: &[&str]) -> String {
        let f = d.frame(size, &lines(output), &lines(region));
        e.feed(&f);
        f
    }

    fn count(rows: &[String], what: &str) -> usize {
        rows.iter().filter(|r| r.contains(what)).count()
    }

    #[test]
    fn output_scrolls_and_the_region_stays_at_the_bottom() {
        let (mut d, mut e) = (Display::new(1), Emu::new(H, W));
        for i in 0..40 {
            let (a, b) = (format!("verdict-{i}-a"), format!("verdict-{i}-b"));
            show(
                &mut d,
                &mut e,
                (W, H),
                &[&a, &b],
                &["HEADER", "run", "gist"],
            );
        }
        let rows = e.rows();
        assert_eq!(&rows[H - 3..], ["HEADER", "run", "gist"]);
        // the last verdicts, in order, directly above the region
        let tail: Vec<String> = (36..40)
            .flat_map(|i| [format!("verdict-{i}-a"), format!("verdict-{i}-b")])
            .collect();
        assert_eq!(rows[H - 3 - tail.len()..H - 3], tail[..]);
        assert_eq!(count(&rows, "HEADER"), 1);
    }

    #[test]
    fn the_region_comes_down_on_close() {
        let (mut d, mut e) = (Display::new(1), Emu::new(H, W));
        for i in 0..5 {
            show(
                &mut d,
                &mut e,
                (W, H),
                &[&format!("verdict-{i}")],
                &["HEADER", "run"],
            );
        }
        e.feed(&d.close());
        let rows = e.rows();
        assert_eq!(count(&rows, "HEADER"), 0);
        assert_eq!(
            rows[..5],
            (0..5).map(|i| format!("verdict-{i}")).collect::<Vec<_>>()[..]
        );
        // the margin is gone: the cursor below the output scrolls the screen
        assert_eq!(e.cursor(), (5, 0));
    }

    #[test]
    fn growth_does_not_eat_output() {
        let (mut d, mut e) = (Display::new(1), Emu::new(H, W));
        show(&mut d, &mut e, (W, H), &["verdict-0"], &["HEADER"]);
        show(
            &mut d,
            &mut e,
            (W, H),
            &["verdict-1"],
            &["HEADER", "a", "b", "c"],
        );
        show(&mut d, &mut e, (W, H), &[], &["HEADER"]);
        e.feed(&d.close());
        let rows = e.rows();
        assert_eq!(rows[..2], ["verdict-0", "verdict-1"]);
        assert_eq!(count(&rows, "HEADER") + count(&rows, " a"), 0);
    }

    /// Output continues right under the prompt, not at the bottom.
    #[test]
    fn output_starts_at_the_origin() {
        let (mut d, mut e) = (Display::new(4), Emu::new(H, W));
        show(&mut d, &mut e, (W, H), &["v0", "v1"], &["HEADER", "run"]);
        assert_eq!(e.rows()[3..7], ["v0", "v1", "HEADER", "run"]);
        assert!(e.rows()[7..].iter().all(|r| r.is_empty()));
    }

    #[test]
    fn a_resize_keeps_one_region() {
        let (mut d, mut e) = (Display::new(1), Emu::new(H, W));
        for i in 0..10 {
            show(
                &mut d,
                &mut e,
                (W, H),
                &[&format!("verdict-{i}")],
                &["HEADER", &format!("run-{i}")],
            );
        }
        e.resize(8, 40);
        for i in 10..16 {
            show(
                &mut d,
                &mut e,
                (40, 8),
                &[&format!("verdict-{i}")],
                &["HEADER", &format!("run-{i}")],
            );
        }
        // two frames let the size settle
        show(&mut d, &mut e, (40, 8), &[], &["HEADER", "run-16"]);
        let rows = e.rows();
        assert_eq!(count(&rows, "HEADER"), 1);
        assert_eq!(&rows[6..], ["HEADER", "run-16"]);
        assert_eq!(rows[5], "verdict-15");
    }

    /// While the size changes every frame nothing is erased and the region
    /// stays hidden, then the screen is rebuilt without stale copies.
    #[test]
    fn a_resize_burst_never_erases() {
        let (mut d, mut e) = (Display::new(1), Emu::new(H, W));
        show(
            &mut d,
            &mut e,
            (W, H),
            &["verdict-0", "verdict-1"],
            &["HEADER", "run"],
        );
        let mut burst = String::new();
        for (i, rows) in [11, 10, 9].into_iter().enumerate() {
            e.resize(rows, W);
            burst += &show(
                &mut d,
                &mut e,
                (W, rows),
                &[&format!("verdict-{}", 2 + i)],
                &["HEADER", "run"],
            );
        }
        assert!(!burst.contains("HEADER"), "{burst:?}");
        assert!(
            !burst.contains("\x1b[J") && !burst.contains("\x1b[2J"),
            "{burst:?}"
        );
        show(&mut d, &mut e, (W, 9), &["verdict-5"], &["HEADER", "run"]);
        show(&mut d, &mut e, (W, 9), &["verdict-6"], &["HEADER", "run"]);
        let rows = e.rows();
        let shown: Vec<&String> = rows.iter().filter(|r| r.starts_with("verdict-")).collect();
        assert_eq!(
            shown,
            (0..7)
                .map(|i| format!("verdict-{i}"))
                .collect::<Vec<_>>()
                .iter()
                .collect::<Vec<_>>()
        );
        assert_eq!(rows.iter().position(|r| r == "HEADER"), Some(shown.len()));
        assert_eq!(count(&rows, "HEADER"), 1);
    }

    /// A region that twitches between sizes holds its top still.
    #[test]
    fn the_region_keeps_its_rows_until_a_smaller_size_lasts() {
        let (mut d, mut e) = (Display::new(1), Emu::new(H, W));
        let rows = |n: usize| -> Vec<String> { (0..n).map(|i| format!("run-{i}")).collect() };
        let fill: Vec<String> = (0..H).map(|i| format!("out-{i}")).collect();
        e.feed(&d.frame((W, H), &fill, &rows(8)));
        let top = d.top();
        assert_eq!(top, H - 7);
        for n in [3, 8, 2, 6, 3, 8, 4] {
            e.feed(&d.frame((W, H), &[], &rows(n)));
            assert_eq!(d.top(), top);
        }
        for _ in 0..SHRINK_AFTER {
            e.feed(&d.frame((W, H), &[], &rows(3)));
        }
        // the largest of the calm frames is what stays
        assert_eq!(d.height, 4);
        e.feed(&d.frame((W, H), &["next".into()], &rows(3)));
        assert_eq!(e.rows()[top - 1], "next");
        e.feed(&d.frame((W, H), &[], &[]));
        assert!(e.rows().iter().all(|r| !r.starts_with("run-")));
    }

    /// The region changes height all the time while builds start and end. Output
    /// that sits where the region grows must scroll up, not be drawn over.
    #[test]
    fn a_region_that_grows_and_shrinks_loses_no_output() {
        let (mut d, mut e) = (Display::new(1), Emu::new(H, W));
        let mut n = 0;
        for step in 0..60 {
            let region: Vec<String> = (0..[2, 8, 3, 9, 2][step % 5])
                .map(|i| format!("run-{i}"))
                .collect();
            let output: Vec<String> = (0..2)
                .map(|_| {
                    n += 1;
                    format!("verdict-{n}")
                })
                .collect();
            e.feed(&d.frame((W, H), &output, &region));
        }
        let rows = e.rows();
        let flow: Vec<&String> = rows.iter().take_while(|r| !r.starts_with("run-")).collect();
        let numbers: Vec<usize> = flow
            .iter()
            .map(|r| {
                r.strip_prefix("verdict-")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            })
            .collect();
        assert_eq!(numbers.last(), Some(&n), "{rows:?}");
        assert!(
            numbers.windows(2).all(|w| w[1] == w[0] + 1),
            "blank or missing rows in {rows:?}"
        );
    }

    #[test]
    fn wrapped_output_does_not_corrupt_the_region() {
        let (mut d, mut e) = (Display::new(1), Emu::new(H, W));
        let long = format!("verdict-long {}", "x".repeat(2 * W));
        for _ in 0..5 {
            show(&mut d, &mut e, (W, H), &[&long], &["HEADER", "run"]);
        }
        let rows = e.rows();
        assert_eq!(&rows[H - 2..], ["HEADER", "run"]);
        assert_eq!(count(&rows, "HEADER"), 1);
        // three rows a line, three whole lines fit above the region
        assert_eq!(
            rows.iter()
                .filter(|r| r.starts_with("verdict-long"))
                .count(),
            3
        );
        assert!(rows[H - 3].chars().all(|c| c == 'x'));
    }

    #[test]
    fn a_full_width_line_keeps_its_last_column() {
        let (mut d, mut e) = (Display::new(1), Emu::new(H, W));
        let full = "y".repeat(W);
        show(&mut d, &mut e, (W, H), &[&full], &["HEADER"]);
        assert_eq!(e.rows()[0], full);
    }

    #[test]
    fn a_frame_is_one_synchronized_write() {
        let mut d = Display::new(1);
        let f = d.frame((W, H), &lines(&["a"]), &lines(&["R"]));
        assert!(f.starts_with(SYNC_START) && f.ends_with(SYNC_END), "{f:?}");
        assert_eq!(f.matches(SYNC_START).count(), 1);
    }

    #[test]
    fn a_region_taller_than_the_screen_is_clamped() {
        let (mut d, mut e) = (Display::new(1), Emu::new(5, W));
        let big: Vec<String> = (0..20).map(|i| format!("r{i}")).collect();
        let big: Vec<&str> = big.iter().map(String::as_str).collect();
        show(&mut d, &mut e, (W, 5), &["out"], &big);
        let rows = e.rows();
        assert_eq!(rows[0], "out");
        // the region floats under the output until that reaches the bottom
        assert_eq!(rows[1..4], ["r0", "r1", "r2"]);
        assert!(rows[4].is_empty());
    }

    #[test]
    fn colour_and_width() {
        assert_eq!(visible_width("\x1b[32mabc\x1b[0m"), 3);
        assert_eq!(fit("abcd", 4), "abcd");
        assert_eq!(fit("\x1b[32mab\x1b[0m", 4), "\x1b[32mab\x1b[0m");
        assert_eq!(
            fit("\x1b[32mabcdef\x1b[0m", 4),
            "\x1b[32mabcd\x1b[0m\x1b[0m"
        );
        assert_eq!(fit("\x1b[2mabcdef", 4), "\x1b[2mabcd\x1b[0m");
    }
}
