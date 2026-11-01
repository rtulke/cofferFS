//! A plain progress line on stderr for long copies (`coffer backup`):
//!
//! ```text
//! [=============>                ]  47%  7.1 GiB / 15.0 GiB  108.3 MiB/s  ETA 1:13
//! ```
//!
//! No colours and no cursor movement beyond a carriage return, so it looks
//! the same on any terminal. Shown only when stderr is a terminal: a backup
//! from cron, or with stderr redirected to a log, prints nothing extra.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

/// Redraws per second are capped: a terminal over SSH shouldn't spend more
/// on the progress line than on the copy.
const REDRAW_EVERY: Duration = Duration::from_millis(100);

/// The bar itself never gets wider than this, however wide the terminal.
const MAX_BAR: usize = 40;

pub struct Bar {
    enabled: bool,
    start: Instant,
    last_draw: Option<Instant>,
    /// The cursor sits at the end of a progress line that still needs its newline.
    open_line: bool,
    /// Length of the last line drawn, to blank out what a shorter one leaves over.
    last_len: usize,
    /// Any report at all - a mount daemon from before progress reports sends none.
    reported: bool,
    done: u64,
    total: u64,
}

impl Bar {
    pub fn new() -> Bar {
        Bar {
            enabled: std::io::stderr().is_terminal(),
            start: Instant::now(),
            last_draw: None,
            open_line: false,
            last_len: 0,
            reported: false,
            done: 0,
            total: 0,
        }
    }

    /// `done` of `total` bytes copied. `done` may go down again - SQLite
    /// restarts a backup whose source changed under it.
    pub fn update(&mut self, done: u64, total: u64) {
        self.done = done;
        self.total = total;
        self.reported = true;
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if self.last_draw.is_some_and(|t| now.duration_since(t) < REDRAW_EVERY) {
            return;
        }
        self.last_draw = Some(now);
        self.draw(false);
    }

    /// Draws the final state - with the time taken instead of an ETA - and
    /// ends the line.
    pub fn finish(&mut self) {
        if !self.enabled || !self.reported {
            return;
        }
        self.draw(true);
        eprintln!();
        self.open_line = false;
    }

    fn draw(&mut self, finished: bool) {
        let line = render(self.done, self.total, self.start.elapsed(), finished, term_width());
        let pad = self.last_len.saturating_sub(line.len());
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r{line}{:pad$}", "");
        let _ = err.flush();
        self.last_len = line.len();
        self.open_line = true;
    }
}

impl Drop for Bar {
    /// A copy that fails half-way leaves the cursor at the end of the
    /// progress line; the error message after it gets a line of its own.
    fn drop(&mut self) {
        if self.open_line {
            eprintln!();
        }
    }
}

/// One progress line, at most `width - 1` columns: writing the last column
/// makes some terminals wrap, and the carriage return would then redraw
/// only the wrapped part.
fn render(done: u64, total: u64, elapsed: Duration, finished: bool, width: usize) -> String {
    let done = done.min(total);
    let pct = if total == 0 { 100 } else { done * 100 / total };
    let secs = elapsed.as_secs_f64();
    let rate = if secs > 0.0 { done as f64 / secs } else { 0.0 };
    let time = if finished {
        format_duration(elapsed.as_secs())
    } else if rate > 0.0 {
        format!("ETA {}", format_duration(((total - done) as f64 / rate) as u64))
    } else {
        "ETA --:--".to_string()
    };
    // Fixed field widths, so the bar keeps its width as the numbers grow:
    // the copied size as wide as the total, the rate as wide as "1023.9 KiB",
    // the time as wide as "ETA 10:02:03".
    let total_size = crate::human_size(total);
    let text = format!(
        "{pct:>3}%  {done:>w$} / {total_size}  {rate:>10}/s  {time:<12}",
        done = crate::human_size(done),
        w = total_size.len(),
        rate = crate::human_size(rate as u64),
    );
    // "[" + bar + "] " in front of the text; below 10 columns of bar it is
    // left out rather than squeezed into something unreadable.
    let room = width.saturating_sub(1 + text.len() + 3);
    let text = text.trim_end();
    if room < 10 {
        return text.to_string();
    }
    let bar_width = room.min(MAX_BAR);
    let filled = if total == 0 { bar_width } else { (bar_width as u64 * done / total) as usize };
    let mut bar = "=".repeat(filled);
    if filled < bar_width {
        bar.push('>');
    }
    format!("[{bar:<bar_width$}] {text}")
}

/// 0:07, 12:34, 1:02:03
fn format_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Columns of the terminal on stderr; 80 when it won't say.
fn term_width() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes one winsize into the struct we hand it.
    if unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
        usize::from(ws.ws_col)
    } else {
        80
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn half_way() {
        let line = render(5 * GIB, 10 * GIB, Duration::from_secs(50), false, 80);
        let bar = format!("{}>{}", "=".repeat(11), " ".repeat(11));
        assert_eq!(line, format!("[{bar}]  50%   5.0 GiB / 10.0 GiB   102.4 MiB/s  ETA 0:50"));
    }

    #[test]
    fn the_bar_keeps_its_width() {
        let ends: Vec<usize> = [0, GIB / 100, GIB, 5 * GIB, 10 * GIB - 1]
            .iter()
            .enumerate()
            .map(|(i, &done)| render(done, 10 * GIB, Duration::from_secs(1 + i as u64 * 40), false, 80).find(']').unwrap())
            .collect();
        assert!(ends.windows(2).all(|w| w[0] == w[1]), "{ends:?}");
        let finished = render(10 * GIB, 10 * GIB, Duration::from_secs(200), true, 80);
        assert_eq!(finished.find(']'), Some(ends[0]));
    }

    #[test]
    fn finished_shows_the_time_taken() {
        let line = render(10 * GIB, 10 * GIB, Duration::from_secs(3723), true, 120);
        assert!(line.starts_with(&format!("[{}] 100%", "=".repeat(MAX_BAR))), "{line}");
        assert!(line.ends_with("  1:02:03"), "{line}");
    }

    #[test]
    fn never_reaches_the_last_column() {
        for width in [70, 79, 80, 81, 200] {
            let line = render(GIB / 3, 10 * GIB, Duration::from_secs(7), false, width);
            assert!(line.starts_with('['), "{width}: {line}");
            assert!(line.len() < width, "{width}: {line}");
        }
        // Too narrow for a bar: the numbers alone.
        assert!(render(GIB, 10 * GIB, Duration::from_secs(7), false, 30).starts_with(" 10%"));
    }

    #[test]
    fn nothing_copied_yet() {
        let line = render(0, 10 * GIB, Duration::ZERO, false, 80);
        assert!(line.starts_with("[>"), "{line}");
        assert!(line.ends_with("ETA --:--"), "{line}");
        // An empty source is done the moment it starts.
        assert!(render(0, 0, Duration::ZERO, true, 80).contains("100%"));
    }
}
