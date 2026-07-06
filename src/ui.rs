//! Full-screen ratatui front-end: 10 EQ band sliders plus bass/treble
//! columns, a status line (input, rates, device, level meter, elapsed
//! time) and a key-hint bar at the bottom.
//!
//! Renders to **stderr** so stdin can stay a PCM pipe; crossterm reads
//! keys from /dev/tty when stdin is not a terminal.

use std::io::{self, BufWriter};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::audio::{AudioStatus, STATE_ENDED, STATE_STREAMING};
use crate::equalizer::Equalizer;
use crate::presets::PRESETS;
use crate::settings::EQ_BANDS;

/// Synthwave '84 palette (Robb Owen's VS Code theme), shared with the
/// clap help styles in `main.rs`.
pub mod theme {
    use ratatui::style::Color;

    pub const PINK: Color = Color::Rgb(0xff, 0x7e, 0xdb); // neon magenta — accents, borders
    pub const CYAN: Color = Color::Rgb(0x36, 0xf9, 0xf6); // electric cyan — bars
    pub const YELLOW: Color = Color::Rgb(0xfe, 0xde, 0x5d); // sun yellow — selection
    pub const ORANGE: Color = Color::Rgb(0xff, 0x8b, 0x39); // sunset orange — elapsed time
    pub const GREEN: Color = Color::Rgb(0x72, 0xf1, 0xb8); // mint green — meters, playing
    pub const RED: Color = Color::Rgb(0xfe, 0x44, 0x50); // alarm red — errors
    pub const COMMENT: Color = Color::Rgb(0x84, 0x8b, 0xbd); // faded violet — labels, hints
    pub const DIM: Color = Color::Rgb(0x49, 0x54, 0x95); // deep violet — inactive/axis
}

use theme::*;

/// Gain range in dB the vertical sliders map onto (± this many dB).
const RANGE_DB: i32 = 24;

/// Selectable columns: the 10 EQ bands, then bass, then treble.
const BASS_COL: usize = EQ_BANDS;
const TOTAL_COLS: usize = EQ_BANDS + 2;

const METER_WIDTH: usize = 8;

/// Static facts about the running pipeline, for the status line.
pub struct StreamInfo {
    pub input: String,
    pub format: String,
    pub in_rate: u32,
    pub out_rate: u32,
    pub device: String,
}

struct App {
    selected: usize,
    /// Index into [`PRESETS`] of the last applied preset; cleared by any
    /// manual band edit.
    preset: Option<usize>,
    quit: bool,
    /// Unsaved changes pending the debounced write in the event loop.
    dirty: bool,
    /// Displayed meter levels with decay, in i16 peak units.
    meter_l: f32,
    meter_r: f32,
}

pub fn run(status: Arc<AudioStatus>, info: StreamInfo, preset: Option<usize>) -> Result<()> {
    enable_raw_mode()?;
    execute!(io::stderr(), EnterAlternateScreen)?;
    // Restore the terminal even if the draw loop panics.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stderr(), LeaveAlternateScreen);
        original_hook(panic);
    }));

    // BufWriter: stderr is unbuffered, so a bare backend would issue one
    // syscall per styled cell and make every frame visibly slow.
    let mut terminal = Terminal::new(CrosstermBackend::new(BufWriter::new(io::stderr())))?;
    let result = event_loop(&mut terminal, &status, &info, preset);

    disable_raw_mode()?;
    execute!(io::stderr(), LeaveAlternateScreen)?;
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<BufWriter<io::Stderr>>>,
    status: &AudioStatus,
    info: &StreamInfo,
    preset: Option<usize>,
) -> Result<()> {
    let mut app = App {
        selected: 0,
        preset,
        quit: false,
        dirty: false,
        meter_l: 0.0,
        meter_r: 0.0,
    };
    let mut last_edit = Instant::now();

    while !app.quit {
        // Peak-hold with decay: take what the audio thread accumulated
        // since the last frame, then fall back slowly.
        let decay = 1800.0;
        app.meter_l = (status.peak_l.swap(0, Ordering::Relaxed) as f32).max(app.meter_l - decay);
        app.meter_r = (status.peak_r.swap(0, Ordering::Relaxed) as f32).max(app.meter_r - decay);

        terminal.draw(|frame| draw(frame, &app, status, info))?;

        // Block until the next event (or the meter tick), then drain the
        // whole queue before redrawing — otherwise a held-down key produces
        // repeats faster than one draw-per-event can consume and the UI
        // lags behind the keyboard.
        if event::poll(Duration::from_millis(50))? {
            loop {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        handle_key(&mut app, key);
                        last_edit = Instant::now();
                    }
                    _ => {}
                }
                if app.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }

        // Debounce disk writes: persist once the user pauses instead of
        // rewriting the TOML on every keystroke.
        if app.dirty && last_edit.elapsed() > Duration::from_millis(400) {
            Equalizer::global().save();
            app.dirty = false;
        }
    }
    if app.dirty {
        Equalizer::global().save();
    }
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) {
    let eq = Equalizer::global();
    let coarse = key.modifiers.contains(KeyModifiers::SHIFT);

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.quit = true;
        return;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Left | KeyCode::Char('h') => app.selected = app.selected.saturating_sub(1),
        KeyCode::Right | KeyCode::Char('l') => {
            app.selected = (app.selected + 1).min(TOTAL_COLS - 1)
        }
        KeyCode::Up | KeyCode::Down | KeyCode::Char('k' | 'j' | '+' | '=' | '-') => {
            let sign = match key.code {
                KeyCode::Up | KeyCode::Char('k' | '+' | '=') => 1,
                _ => -1,
            };
            match app.selected {
                // Bands are in tenths of dB: 0.5 dB fine, 2 dB coarse.
                0..BASS_COL => {
                    let step = if coarse { 20 } else { 5 };
                    eq.adjust_band_gain(app.selected, sign * step);
                    app.preset = None;
                }
                // Tone shelves are in whole dB: 1 dB fine, 4 dB coarse.
                BASS_COL => eq.adjust_bass(sign * if coarse { 4 } else { 1 }),
                _ => eq.adjust_treble(sign * if coarse { 4 } else { 1 }),
            }
            app.dirty = true;
        }
        // Direct bass/treble +/- without moving the selection.
        KeyCode::Char('b' | 'B') => {
            eq.adjust_bass(if key.code == KeyCode::Char('b') {
                1
            } else {
                -1
            });
            app.dirty = true;
        }
        KeyCode::Char('t' | 'T') => {
            eq.adjust_treble(if key.code == KeyCode::Char('t') {
                1
            } else {
                -1
            });
            app.dirty = true;
        }
        KeyCode::Char(' ') | KeyCode::Char('e') => {
            eq.set_enabled(!eq.is_enabled());
            app.dirty = true;
        }
        KeyCode::Char('p' | 'P') => {
            let dir: isize = if key.code == KeyCode::Char('p') {
                1
            } else {
                -1
            };
            let next = match app.preset {
                Some(i) => (i as isize + dir).rem_euclid(PRESETS.len() as isize) as usize,
                None => {
                    if dir > 0 {
                        0
                    } else {
                        PRESETS.len() - 1
                    }
                }
            };
            eq.set_band_gains_db(&PRESETS[next].gains_db);
            eq.set_enabled(true);
            app.preset = Some(next);
            app.dirty = true;
        }
        KeyCode::Char('0') | KeyCode::Char('r') => {
            eq.reset_gains();
            app.preset = None;
            app.dirty = true;
        }
        KeyCode::Char('s') => {
            eq.save();
            app.dirty = false;
        }
        _ => {}
    }
}

fn draw(frame: &mut ratatui::Frame, app: &App, status: &AudioStatus, info: &StreamInfo) {
    // Breathing room: keep the bordered block off the window edges and
    // separate the status/help lines from it and from each other.
    let [sliders_area, _, status_area, _, help_area] = Layout::vertical([
        Constraint::Min(8),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .horizontal_margin(2)
    .vertical_margin(1)
    .areas(frame.area());

    let eq = Equalizer::global();
    let enabled = eq.is_enabled();

    // Bands are stored in tenths of dB, the tone shelves in whole dB;
    // normalize everything to tenths for the slider columns.
    let mut columns: Vec<SliderColumn> = eq
        .bands()
        .iter()
        .map(|b| SliderColumn {
            gain_tenths: b.gain,
            label: fmt_hz(b.cutoff),
            dimmed: !enabled,
        })
        .collect();
    columns.push(SliderColumn {
        gain_tenths: eq.bass() * 10,
        label: "Bass".to_string(),
        dimmed: eq.bass() == 0,
    });
    columns.push(SliderColumn {
        gain_tenths: eq.treble() * 10,
        label: "Treble".to_string(),
        dimmed: eq.treble() == 0,
    });

    let preset_name = app.preset.map(|i| PRESETS[i].name).unwrap_or("custom");
    let title = format!(
        " Equalizer — {} (±{RANGE_DB} dB) — {preset_name} ",
        if enabled { "ON" } else { "OFF" },
    );
    let block = Block::new()
        .borders(Borders::ALL)
        .title(title)
        .title_alignment(Alignment::Center)
        .border_style(if enabled {
            Style::default().fg(PINK)
        } else {
            Style::default().fg(DIM)
        });
    let inner = block.inner(sliders_area);
    frame.render_widget(block, sliders_area);

    if inner.height < 5 || inner.width < 30 {
        frame.render_widget(Paragraph::new("Terminal too small"), inner);
    } else {
        frame.render_widget(
            EqSliders {
                columns: &columns,
                selected: app.selected,
            },
            inner,
        );
    }

    frame.render_widget(status_line(app, status, info), status_area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "←/→ band  ↑/↓ gain ±0.5 dB (shift ±2)  b/B bass ±  t/T treble ±  space on/off  p/P preset  0 reset  q quit",
            Style::default().fg(COMMENT),
        )))
        .alignment(Alignment::Center),
        help_area,
    );
}

fn status_line<'a>(app: &App, status: &AudioStatus, info: &'a StreamInfo) -> Paragraph<'a> {
    let state = status.state.load(Ordering::Acquire);
    let (state_text, state_color) = if let Some(err) = status.error.lock().unwrap().clone() {
        (format!("✖ {err}"), RED)
    } else {
        match state {
            STATE_STREAMING => ("▶ playing".to_string(), GREEN),
            STATE_ENDED => ("■ input ended".to_string(), YELLOW),
            _ => ("… waiting for input".to_string(), COMMENT),
        }
    };

    let rate = if info.in_rate == info.out_rate {
        format!("{} Hz", info.in_rate)
    } else {
        format!("{}→{} Hz", info.in_rate, info.out_rate)
    };
    let seconds = status.frames_played.load(Ordering::Relaxed) / info.out_rate.max(1) as u64;
    let elapsed = format!("{:02}:{:02}", seconds / 60, seconds % 60);

    let spans = vec![
        Span::styled(
            format!(" {} ", shorten(&info.input, 32)),
            Style::default().fg(PINK).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} {rate} ", info.format),
            Style::default().fg(COMMENT),
        ),
        Span::styled(format!("→ {} ", info.device), Style::default().fg(COMMENT)),
        Span::styled(format!(" {state_text} "), Style::default().fg(state_color)),
        Span::styled(format!(" {elapsed} "), Style::default().fg(ORANGE)),
        Span::styled(" L", Style::default().fg(COMMENT)),
        Span::styled(meter_bar(app.meter_l), Style::default().fg(GREEN)),
        Span::styled(" R", Style::default().fg(COMMENT)),
        Span::styled(meter_bar(app.meter_r), Style::default().fg(GREEN)),
    ];
    Paragraph::new(Line::from(spans))
}

/// Fixed-width level meter mapping −48…0 dBFS onto `METER_WIDTH` cells
/// with ⅛-cell resolution.
fn meter_bar(peak: f32) -> String {
    const EIGHTHS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let db = if peak < 1.0 {
        -96.0
    } else {
        20.0 * (peak / 32768.0).log10()
    };
    let norm = ((db + 48.0) / 48.0).clamp(0.0, 1.0);
    let cells_8 = (norm * (METER_WIDTH * 8) as f32).round() as usize;
    let mut bar = "█".repeat(cells_8 / 8);
    if cells_8 % 8 > 0 {
        bar.push(EIGHTHS[cells_8 % 8 - 1]);
    }
    let used = cells_8 / 8 + usize::from(cells_8 % 8 > 0);
    bar.push_str(&" ".repeat(METER_WIDTH - used));
    bar
}

/// One slider column, drawn with vertical block characters: a dB value
/// above the bar, a Hz/kHz (or "Bass"/"Treble") label below, and a
/// highlight on the selected column.
struct SliderColumn {
    /// Gain in tenths of dB.
    gain_tenths: i32,
    /// Text under the bar.
    label: String,
    /// Draw the bar muted (EQ off for bands, 0 dB for the tone shelves).
    dimmed: bool,
}

struct EqSliders<'a> {
    columns: &'a [SliderColumn],
    selected: usize,
}

impl Widget for EqSliders<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let n = self.columns.len();
        if n == 0 || area.width < 8 || area.height < 5 {
            return;
        }
        let col_w = (area.width / n as u16).max(4);
        let center_row = area.y + area.height / 2;

        // Reserve the top row for the "+N.N dB" text, the bottom row for
        // the label text; every row in between draws the bar.
        let bar_top = area.y + 1;
        let bar_bot = area.y + area.height.saturating_sub(2);
        if bar_bot <= bar_top {
            return;
        }
        let half_h = (bar_bot - bar_top) as f32 / 2.0;

        // Bars are drawn several cells wide (as much as fits the column,
        // capped) around a thin center track.
        let bar_w = (col_w.saturating_sub(2)).clamp(1, 3);

        for (i, column) in self.columns.iter().enumerate() {
            let col_x = area.x + (i as u16) * col_w;
            let bar_x = col_x + col_w / 2;
            let bar_x0 = col_x + (col_w - bar_w) / 2;
            if bar_x >= area.right() {
                break;
            }

            // Separate the tone shelves from the EQ bands visually.
            if i == EQ_BANDS {
                for r in area.y..area.y + area.height {
                    if let Some(cell) = buf.cell_mut((col_x, r)) {
                        cell.set_char('┆').set_style(Style::default().fg(DIM));
                    }
                }
            }

            let gain_db = column.gain_tenths as f32 / 10.0;
            // Signed bar extent in rows, split into whole cells plus a
            // ⅛-cell tip: one full row is ~2 dB at typical heights, so
            // without partial blocks a 0.5 dB keypress often wouldn't move
            // the bar at all and it would look laggy.
            let offset =
                gain_db.clamp(-(RANGE_DB as f32), RANGE_DB as f32) / RANGE_DB as f32 * half_h;
            let mut full = offset.abs().floor() as u16;
            let mut eighths = (offset.abs().fract() * 8.0).round() as u16;
            if eighths == 8 {
                full += 1;
                eighths = 0;
            }

            let is_sel = i == self.selected;
            let bar_color = if column.dimmed {
                DIM
            } else if is_sel {
                YELLOW
            } else {
                CYAN
            };

            // Zero-dB axis through every column so the user can eyeball
            // who is pushed above / below flat: a thin vertical track plus
            // a bar-wide tick on the axis row.
            for r in bar_top..=bar_bot {
                if r == center_row {
                    for x in bar_x0..bar_x0 + bar_w {
                        if let Some(cell) = buf.cell_mut((x, r)) {
                            cell.set_char('─').set_style(Style::default().fg(DIM));
                        }
                    }
                } else if let Some(cell) = buf.cell_mut((bar_x, r)) {
                    cell.set_char('│').set_style(Style::default().fg(DIM));
                }
            }

            // Fill from the zero axis toward the gain position, the full
            // bar width, finishing with a partial-block tip. Above the
            // axis the bottom-eighth glyphs work directly; below it the
            // tip must fill top-down, so draw the complementary glyph
            // REVERSED (the glyph area becomes background, the rest bar
            // color).
            const TIP: [char; 7] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇'];
            let fill = Style::default().fg(bar_color).add_modifier(Modifier::BOLD);
            let mut paint = |r: u16, ch: char, style: Style| {
                for x in bar_x0..bar_x0 + bar_w {
                    if let Some(cell) = buf.cell_mut((x, r)) {
                        cell.set_char(ch).set_style(style);
                    }
                }
            };
            if offset >= 0.0 {
                let full = full.min(center_row - bar_top);
                for r in center_row - full..=center_row {
                    paint(r, '█', fill);
                }
                if eighths > 0 && center_row - full > bar_top {
                    paint(center_row - full - 1, TIP[eighths as usize - 1], fill);
                }
            } else {
                let full = full.min(bar_bot - center_row);
                for r in center_row..=center_row + full {
                    paint(r, '█', fill);
                }
                if eighths > 0 && center_row + full < bar_bot {
                    paint(
                        center_row + full + 1,
                        TIP[(8 - eighths) as usize - 1],
                        fill.add_modifier(Modifier::REVERSED),
                    );
                }
            }

            let label_style = if is_sel {
                Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)
            } else if column.dimmed {
                Style::default().fg(COMMENT)
            } else {
                Style::default().fg(PINK)
            };

            Paragraph::new(Span::styled(format!("{:+.1} dB", gain_db), label_style))
                .alignment(Alignment::Center)
                .render(Rect::new(col_x, area.y, col_w, 1), buf);

            let label_row = area.y + area.height - 1;
            Paragraph::new(Span::styled(column.label.clone(), label_style))
                .alignment(Alignment::Center)
                .render(Rect::new(col_x, label_row, col_w, 1), buf);
        }
    }
}

/// Keep long input paths from flooding the status line: `…` + the last
/// `max - 1` characters.
fn shorten(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - (max - 1)).collect();
    format!("…{tail}")
}

/// Format a band cutoff with its unit: `63 Hz`, `1 kHz`, `2.5 kHz`.
fn fmt_hz(hz: i32) -> String {
    if hz < 1000 {
        format!("{} Hz", hz)
    } else if hz % 1000 == 0 {
        format!("{} kHz", hz / 1000)
    } else {
        format!("{:.1} kHz", hz as f32 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_bar_is_fixed_width() {
        for peak in [0.0, 1.0, 100.0, 5000.0, 32768.0] {
            assert_eq!(meter_bar(peak).chars().count(), METER_WIDTH, "peak {peak}");
        }
    }

    #[test]
    fn meter_bar_monotonic() {
        let silent = meter_bar(0.0);
        let full = meter_bar(32768.0);
        assert!(silent.trim().is_empty());
        assert_eq!(full, "█".repeat(METER_WIDTH));
    }

    #[test]
    fn shorten_keeps_tail() {
        assert_eq!(shorten("stdin", 32), "stdin");
        let long = "/very/long/path/to/some/audio.fifo";
        let short = shorten(long, 16);
        assert_eq!(short.chars().count(), 16);
        assert!(short.starts_with('…') && short.ends_with("audio.fifo"));
    }

    #[test]
    fn hz_formatting() {
        assert_eq!(fmt_hz(63), "63 Hz");
        assert_eq!(fmt_hz(1000), "1 kHz");
        assert_eq!(fmt_hz(2500), "2.5 kHz");
        assert_eq!(fmt_hz(16000), "16 kHz");
    }
}
