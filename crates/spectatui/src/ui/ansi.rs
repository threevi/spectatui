//! ANSI escape sequence → ratatui `Line` conversion.
//!
//! Parses a single line of terminal output (which may contain SGR / CSI
//! escape sequences) into a ratatui [`Line`] whose [`Span`]s carry the
//! foreground color, background color, and text modifiers implied by the
//! escape sequences.

use anstyle_parse::{Perform, Params, Parser};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Parse `text` (potentially containing ANSI SGR escapes) into a [`Line`]
/// of styled spans. If the text has no escapes the result is a single
/// default-style span.
pub fn ansi_to_line(text: &str) -> Line<'_> {
    let mut parser: anstyle_parse::Parser = Parser::default();
    let mut performer = SgrPerformer::default();

    for byte in text.bytes() {
        parser.advance(&mut performer, byte);
    }

    // Flush the trailing span (text after the last SGR sequence).
    performer.finish().into()
}

/// Strip all recognized escape sequences from `text`, returning the plain
/// visible characters. Used to render a color-captured snapshot in the
/// plain (dim) style without leaking raw escape bytes.
pub fn strip_ansi(text: &str) -> String {
    let mut parser: anstyle_parse::Parser = Parser::default();
    let mut performer = StripPerformer::default();
    for byte in text.bytes() {
        parser.advance(&mut performer, byte);
    }
    performer.out
}

/// A [`Perform`] impl that throws away everything except printable chars.
#[derive(Default)]
struct StripPerformer {
    out: String,
}

impl Perform for StripPerformer {
    fn print(&mut self, c: char) {
        self.out.push(c);
    }
}

/// A minimal [`Perform`] implementation that accumulates styled spans as
/// SGR sequences arrive.
#[derive(Default)]
struct SgrPerformer {
    spans: Vec<Span<'static>>,
    /// Text accumulated since the last flush.
    buf: String,
    /// Style in effect for the current `buf`.
    style: Style,
}

impl SgrPerformer {
    /// Flush the accumulated buffer as a span, reset it.
    fn flush(&mut self) {
        if !self.buf.is_empty() {
            self.spans.push(Span::styled(std::mem::take(&mut self.buf), self.style));
        }
    }

    /// Finish parsing: flush any trailing text and return the collected spans.
    fn finish(mut self) -> Vec<Span<'static>> {
        self.flush();
        if self.spans.is_empty() {
            // Fully-blank line / all-styled: return a single empty span so
            // callers get a well-formed Line.
            self.spans.push(Span::raw(""));
        }
        self.spans
    }
}

impl Perform for SgrPerformer {
    fn print(&mut self, c: char) {
        self.buf.push(c);
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: u8,
    ) {
        // SGR (Select Graphic Rendition) is the CSI sequence with final byte 'm'.
        if action != b'm' {
            return;
        }
        self.flush();

        let mut iter = params.iter();
        while let Some(paramslice) = iter.next() {
            if paramslice.is_empty() {
                continue;
            }
            let first = paramslice[0];
            match first {
                0 => {
                    // Reset
                    self.style = Style::default();
                }
                1 => {
                    self.style = self.style.add_modifier(Modifier::BOLD);
                }
                2 => {
                    self.style = self.style.add_modifier(Modifier::DIM);
                }
                3 => {
                    self.style = self.style.add_modifier(Modifier::ITALIC);
                }
                4 => {
                    self.style = self.style.add_modifier(Modifier::UNDERLINED);
                }
                7 => {
                    self.style = self.style.add_modifier(Modifier::REVERSED);
                }
                21 => {
                    // Not italic (old) — ignore
                }
                22 => {
                    self.style = self.style.remove_modifier(Modifier::BOLD | Modifier::DIM);
                }
                23 => {
                    self.style = self.style.remove_modifier(Modifier::ITALIC);
                }
                24 => {
                    self.style = self.style.remove_modifier(Modifier::UNDERLINED);
                }
                27 => {
                    self.style = self.style.remove_modifier(Modifier::REVERSED);
                }
                28 => {
                    self.style = self.style.remove_modifier(Modifier::HIDDEN);
                }
                30..=37 => {
                    self.style = self.style.fg(named_color(first - 30));
                }
                38 => {
                    if let Some(c) = extended_color(&paramslice[1..], &mut iter) {
                        self.style = self.style.fg(c);
                    }
                }
                39 => {
                    self.style = self.style.fg(Color::Reset);
                }
                40..=47 => {
                    self.style = self.style.bg(named_color(first - 40));
                }
                48 => {
                    if let Some(c) = extended_color(&paramslice[1..], &mut iter) {
                        self.style = self.style.bg(c);
                    }
                }
                49 => {
                    self.style = self.style.bg(Color::Reset);
                }
                90..=97 => {
                    self.style = self.style.fg(named_color_bright(first - 90));
                }
                100..=107 => {
                    self.style = self.style.bg(named_color_bright(first - 100));
                }
                // 5x blink, 52 hidden, 53 strikethrough — map what ratatui has
                51 => {
                    self.style = self.style.add_modifier(Modifier::HIDDEN);
                }
                53 => {
                    self.style = self.style.add_modifier(Modifier::CROSSED_OUT);
                }
                _ => {}
            }
        }
    }
}

/// Map SGR base index (0-7) to the matching ratatui named color.
fn named_color(idx: u16) -> Color {
    match idx {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::White,
        _ => Color::Gray,
    }
}

/// Map SGR bright index (0-7) to the matching named color (ratatui 0.29 has
/// no Light* variants; bright = base + White/DarkGray for the light ones).
fn named_color_bright(idx: u16) -> Color {
    match idx {
        0 => Color::DarkGray,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        _ => Color::White,
    }
}

/// Parse the extended 256/24-bit color sub-parameters.
///
/// Formats:
///   `5;n`     → ANSI 256-color (`[38, 5, n]`)
///   `2;r;g;b` → true color     (`[38, 2, r, g, b]`)
///
/// The slice `rest` is the portion after the leading `38` or `48`. The
/// caller's `iter` (an `Iterator<Item = &[u16]>`) is consumed as needed
/// for the multi-element form.
///
/// For the in-param form (`38;5;n` in a single param) `rest` is `[5, n]`
/// or `[2, r, g, b]`. For the split-param form (`38`, then separate `5`,
/// then `n`) `rest` is empty and we consume from the iterator.
fn extended_color(rest: &[u16], iter: &mut dyn Iterator<Item = &[u16]>) -> Option<Color> {
    // In-param form: rest = [5, n] or [2, r, g, b]
    if rest.len() >= 2 {
        match rest[0] {
            5 => return Some(Color::Indexed(rest[1] as u8)),
            2 if rest.len() >= 4 => {
                return Some(Color::Rgb(rest[1] as u8, rest[2] as u8, rest[3] as u8))
            }
            _ => return None,
        }
    }
    // Split-param form: consume from iterator
    let mode = iter.next()?.first().copied()?;
    match mode {
        5 => {
            let n = iter.next()?.first().copied()?;
            Some(Color::Indexed(n as u8))
        }
        2 => {
            let r = iter.next()?.first().copied()?;
            let g = iter.next()?.first().copied()?;
            let b = iter.next()?.first().copied()?;
            Some(Color::Rgb(r as u8, g as u8, b as u8))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn plain_text_single_span() {
        let l = ansi_to_line("hello world");
        assert_eq!(render_text(&l), "hello world");
        assert_eq!(l.spans.len(), 1);
        assert_eq!(l.spans[0].style, Style::default());
    }

    #[test]
    fn bold_text() {
        let raw = "\x1b[1mbold\x1b[0m normal";
        let l = ansi_to_line(raw);
        assert_eq!(render_text(&l), "bold normal");
        assert_eq!(l.spans.len(), 2);
        assert!(l.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(l.spans[1].style.add_modifier.is_empty());
    }

    #[test]
    fn fg_color() {
        let raw = "\x1b[31mred\x1b[0m";
        let l = ansi_to_line(raw);
        assert_eq!(l.spans.len(), 1);
        assert_eq!(l.spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn bright_fg() {
        let raw = "\x1b[91mlight red\x1b[0m";
        let l = ansi_to_line(raw);
        // ratatui 0.29 has no Light* variants; bright white→White, bright red→Red, etc.
        assert_eq!(l.spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn bg_color() {
        let raw = "\x1b[44mblue bg\x1b[0m";
        let l = ansi_to_line(raw);
        assert_eq!(l.spans[0].style.bg, Some(Color::Blue));
    }

    #[test]
    fn ansi256_color() {
        let raw = "\x1b[38;5;196mpink\x1b[0m";
        let l = ansi_to_line(raw);
        assert_eq!(l.spans[0].style.fg, Some(Color::Indexed(196)));
    }

    #[test]
    fn truecolor() {
        let raw = "\x1b[38;2;10;20;30mrgb\x1b[0m";
        let l = ansi_to_line(raw);
        assert_eq!(l.spans[0].style.fg, Some(Color::Rgb(10, 20, 30)));
    }

    #[test]
    fn multiple_styles() {
        let raw = "\x1b[1;31mbold red\x1b[0m \x1b[4munder\x1b[0m";
        let l = ansi_to_line(raw);
        assert_eq!(render_text(&l), "bold red under");
        assert_eq!(l.spans.len(), 3);
        assert!(l.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(l.spans[0].style.fg, Some(Color::Red));
        assert!(l.spans[2].style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn empty_input() {
        let l = ansi_to_line("");
        assert_eq!(render_text(&l), "");
    }

    #[test]
    fn no_trailing_reset() {
        // Line ends without a reset — remainder keeps the active style.
        let raw = "plain \x1b[31mred tail";
        let l = ansi_to_line(raw);
        assert_eq!(render_text(&l), "plain red tail");
        assert_eq!(l.spans.len(), 2);
    }

    #[test]
    fn strips_ansi() {
        let raw = "\x1b[1;38;5;196mbold pink\x1b[0m plain";
        assert_eq!(strip_ansi(raw), "bold pink plain");
        assert_eq!(strip_ansi("no escapes"), "no escapes");
    }
}