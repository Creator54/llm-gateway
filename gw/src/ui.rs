//! Terminal output: colors, symbols, tables, prompts.
//!
//! Colors are used only when the stream is a terminal and the environment
//! allows it ([`NO_COLOR`], `TERM=dumb`, `FORCE_COLOR`, `CLICOLOR_FORCE`).
//! Symbols fall back to ASCII without a UTF-8 locale or with `GW_ASCII=1`.

use clap::builder::styling::{AnsiColor, Color, Style, Styles};
use std::io::{IsTerminal, Write};

const GREEN: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
const YELLOW: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));
const RED: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red)));
const BOLD: Style = Style::new().bold();
const DIM: Style = Style::new().dimmed();

/// Default line width when `COLUMNS` is unset.
const DEFAULT_WIDTH: usize = 100;

struct Symbols {
    ok: &'static str,
    warn: &'static str,
    err: &'static str,
    info: &'static str,
    dots: &'static str,
}

const UTF8: Symbols = Symbols {
    ok: "✓",
    warn: "!",
    err: "✗",
    info: "·",
    dots: "…",
};

const ASCII: Symbols = Symbols {
    ok: "+",
    warn: "!",
    err: "x",
    info: "-",
    dots: "...",
};

/// Output configuration resolved from the environment.
#[derive(Clone, Copy)]
pub struct Ui {
    color: bool,
    color_err: bool,
    unicode: bool,
    width: usize,
}

impl Ui {
    /// Resolve color, symbols and width from the current environment.
    pub fn from_env() -> Self {
        let sym = symbols(env("GW_ASCII"), env("LC_ALL"), env("LANG"));
        let width = env("COLUMNS")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|w| *w >= 20)
            .unwrap_or(DEFAULT_WIDTH);
        let (no_color, term) = (env("NO_COLOR"), env("TERM"));
        let (force, force_cli) = (env("FORCE_COLOR"), env("CLICOLOR_FORCE"));
        let color = colors_enabled(
            std::io::stdout().is_terminal(),
            no_color.as_deref(),
            term.as_deref(),
            force.as_deref(),
            force_cli.as_deref(),
        );
        let color_err = colors_enabled(
            std::io::stderr().is_terminal(),
            no_color.as_deref(),
            term.as_deref(),
            force.as_deref(),
            force_cli.as_deref(),
        );
        Self {
            color,
            color_err,
            unicode: sym,
            width,
        }
    }

    /// Fixed configuration; useful when the environment is not the input.
    pub fn custom(color: bool, unicode: bool, width: usize) -> Self {
        Self {
            color,
            color_err: color,
            unicode,
            width,
        }
    }

    fn sym(&self) -> &'static Symbols {
        if self.unicode {
            &UTF8
        } else {
            &ASCII
        }
    }

    fn paint(&self, color: bool, style: Style, text: &str) -> String {
        if color && !style.is_plain() {
            format!("{}{text}{}", style.render(), style.render_reset())
        } else {
            text.to_string()
        }
    }

    /// Line width used by [`kv`](Self::kv) and [`table`](Self::table).
    pub fn width(&self) -> usize {
        self.width
    }

    /// "✓ msg"
    pub fn ok(&self, msg: &str) {
        let line = format!("{} {msg}", self.sym().ok);
        println!("{}", self.paint(self.color, GREEN, &line));
    }

    /// "! msg"
    pub fn warn(&self, msg: &str) {
        let line = format!("{} {msg}", self.sym().warn);
        println!("{}", self.paint(self.color, YELLOW, &line));
    }

    /// "✗ msg", to stderr.
    pub fn err(&self, msg: &str) {
        let line = format!("{} {msg}", self.sym().err);
        eprintln!("{}", self.paint(self.color_err, RED, &line));
    }

    /// "· msg"
    pub fn info(&self, msg: &str) {
        let line = format!("{} {msg}", self.sym().info);
        println!("{}", self.paint(self.color, DIM, &line));
    }

    /// "[n/total] msg"
    pub fn step(&self, n: usize, total: usize, msg: &str) {
        let counter = self.paint(self.color, DIM, &format!("[{n}/{total}]"));
        println!("{counter} {msg}");
    }

    /// Bold heading line.
    pub fn heading(&self, text: &str) {
        println!("{}", self.paint(self.color, BOLD, text));
    }

    /// Dim "hint: ..." line.
    pub fn hint(&self, text: &str) {
        println!("{}", self.paint(self.color, DIM, &format!("hint: {text}")));
    }

    /// Aligned key/value block, keys dim.
    pub fn kv(&self, rows: &[(&str, &str)]) {
        print!("{}", self.render_kv(rows));
    }

    /// Simple aligned table: no box drawing, dim header.
    pub fn table(&self, headers: &[&str], rows: &[&[&str]]) {
        print!("{}", self.render_table(headers, rows));
    }

    /// Print a question and read one line from stdin (empty on EOF).
    pub fn prompt(&self, question: &str) -> String {
        print!("{question} ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line.trim().to_string()
    }

    /// Ask a yes/no question; empty input yields `default`.
    pub fn confirm(&self, question: &str, default: bool) -> bool {
        let suffix = if default { "[Y/n]" } else { "[y/N]" };
        loop {
            match self
                .prompt(&format!("{question} {suffix}"))
                .to_lowercase()
                .as_str()
            {
                "" => return default,
                "y" | "yes" => return true,
                "n" | "no" => return false,
                _ => self.warn("please answer y or n"),
            }
        }
    }

    /// Key/value block; value truncated to the line width.
    pub fn render_kv(&self, rows: &[(&str, &str)]) -> String {
        let key_w = rows.iter().map(|(k, _)| chars(k)).max().unwrap_or(0);
        let avail = self.width.saturating_sub(key_w + 2).max(8);
        let mut out = String::new();
        for (key, value) in rows {
            let pad = " ".repeat(key_w - chars(key) + 2);
            let value = truncate(value, avail, self.sym().dots);
            out.push_str(&format!(
                "{}{pad}{value}\n",
                self.paint(self.color, DIM, key)
            ));
        }
        out
    }

    /// Aligned table, shrunk to the line width. Missing cells render empty.
    pub fn render_table(&self, headers: &[&str], rows: &[&[&str]]) -> String {
        let cols = headers.len();
        if cols == 0 {
            return String::new();
        }
        let cell =
            |row: &[&str], i: usize| -> String { row.get(i).copied().unwrap_or("").to_string() };
        let mut nat: Vec<usize> = headers.iter().map(|h| chars(h)).collect();
        for row in rows {
            for (i, w) in nat.iter_mut().enumerate() {
                *w = (*w).max(chars(&cell(row, i)));
            }
        }
        let widths = fit(&nat, self.width.saturating_sub(2 * (cols - 1)).max(cols));
        let dots = self.sym().dots;

        let mut out = String::new();
        let mut header = Vec::with_capacity(cols);
        for (i, h) in headers.iter().enumerate() {
            header.push(self.paint(self.color, DIM, &pad(h, widths[i], dots)));
        }
        out.push_str(&join(&header));
        for row in rows {
            let cells: Vec<String> = (0..cols)
                .map(|i| pad(&cell(row, i), widths[i], dots))
                .collect();
            out.push_str(&join(&cells));
        }
        out
    }
}

/// Color decision from plain inputs: a forced color wins, then `NO_COLOR`,
/// `TERM=dumb`, and finally whether the stream is a terminal.
pub fn colors_enabled(
    tty: bool,
    no_color: Option<&str>,
    term: Option<&str>,
    force_color: Option<&str>,
    clicolor_force: Option<&str>,
) -> bool {
    if forced(force_color) || forced(clicolor_force) {
        return true;
    }
    if no_color.is_some() {
        return false;
    }
    if term == Some("dumb") {
        return false;
    }
    tty
}

/// Whether to use UTF-8 symbols; `GW_ASCII` forces ASCII.
pub fn use_unicode(gw_ascii: Option<&str>, lc_all: Option<&str>, lang: Option<&str>) -> bool {
    if forced(gw_ascii) {
        return false;
    }
    let locale = lc_all
        .filter(|v| !v.is_empty())
        .or(lang.filter(|v| !v.is_empty()));
    match locale {
        Some(l) => {
            let l = l.to_ascii_uppercase();
            l.contains("UTF-8") || l.contains("UTF8")
        }
        None => false,
    }
}

/// "sk-…abcd": first three and last four characters, nothing for short secrets.
pub fn mask(secret: &str) -> String {
    let len = chars(secret);
    if len == 0 {
        return String::new();
    }
    if len <= 7 {
        return "…".to_string();
    }
    let head: String = secret.chars().take(3).collect();
    let tail: String = secret.chars().skip(len - 4).collect();
    format!("{head}…{tail}")
}

/// Clap help/error styles: bold headers, cyan literals, dim placeholders,
/// red errors.
pub fn styles() -> Styles {
    Styles::styled()
        .header(BOLD)
        .usage(BOLD)
        .literal(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Cyan))))
        .placeholder(DIM)
        .error(RED.bold())
        .valid(GREEN)
        .invalid(YELLOW)
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn symbols(gw_ascii: Option<String>, lc_all: Option<String>, lang: Option<String>) -> bool {
    use_unicode(gw_ascii.as_deref(), lc_all.as_deref(), lang.as_deref())
}

fn forced(value: Option<&str>) -> bool {
    matches!(value, Some(v) if !v.is_empty() && v != "0")
}

fn chars(text: &str) -> usize {
    text.chars().count()
}

fn truncate(text: &str, width: usize, dots: &str) -> String {
    if chars(text) <= width {
        return text.to_string();
    }
    if width <= chars(dots) {
        return text.chars().take(width).collect();
    }
    let keep = width - chars(dots);
    let mut out: String = text.chars().take(keep).collect();
    out.push_str(dots);
    out
}

fn pad(text: &str, width: usize, dots: &str) -> String {
    let text = truncate(text, width, dots);
    let fill = " ".repeat(width - chars(&text));
    format!("{text}{fill}")
}

/// Join padded cells with two spaces, without trailing whitespace.
fn join(cells: &[String]) -> String {
    let line = cells.join("  ");
    format!("{}\n", line.trim_end())
}

/// Shrink natural column widths to `available`, proportionally, largest first.
fn fit(nat: &[usize], available: usize) -> Vec<usize> {
    let total: usize = nat.iter().sum();
    if total <= available {
        return nat.to_vec();
    }
    let mut widths: Vec<usize> = nat
        .iter()
        .map(|&w| (w * available / total).max(1))
        .collect();
    loop {
        let sum: usize = widths.iter().sum();
        if sum <= available {
            return widths;
        }
        let Some(i) = (0..widths.len()).max_by_key(|&i| widths[i]) else {
            return widths;
        };
        if widths[i] <= 1 {
            return widths;
        }
        widths[i] -= 1;
    }
}

/// "✓ msg"
pub fn ok(msg: &str) {
    Ui::from_env().ok(msg);
}

/// "! msg"
pub fn warn(msg: &str) {
    Ui::from_env().warn(msg);
}

/// "✗ msg"
pub fn err(msg: &str) {
    Ui::from_env().err(msg);
}

/// "· msg"
pub fn info(msg: &str) {
    Ui::from_env().info(msg);
}

/// "[n/total] msg"
pub fn step(n: usize, total: usize, msg: &str) {
    Ui::from_env().step(n, total, msg);
}

/// Bold heading.
pub fn heading(text: &str) {
    Ui::from_env().heading(text);
}

/// Dim "hint: ...".
pub fn hint(text: &str) {
    Ui::from_env().hint(text);
}

/// Aligned key/value block.
pub fn kv(rows: &[(&str, &str)]) {
    Ui::from_env().kv(rows);
}

/// Aligned table.
pub fn table(headers: &[&str], rows: &[&[&str]]) {
    Ui::from_env().table(headers, rows);
}

/// Ask a question, returning the raw trimmed answer.
pub fn prompt(question: &str) -> String {
    Ui::from_env().prompt(question)
}

/// Ask a yes/no question.
pub fn confirm(question: &str, default: bool) -> bool {
    Ui::from_env().confirm(question, default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(width: usize) -> Ui {
        Ui::custom(false, true, width)
    }

    #[test]
    fn kv_aligns_values() {
        let out = plain(100).render_kv(&[("provider", "openai"), ("short", "x")]);
        assert_eq!(out.lines().count(), 2);
        let second = out.lines().nth(1).unwrap();
        assert_eq!(second, "short     x");
    }

    #[test]
    fn kv_truncates_long_values() {
        let long = "a".repeat(200);
        let out = plain(30).render_kv(&[("k", &long)]);
        let line = out.lines().next().unwrap();
        assert_eq!(chars(line), 30);
        assert!(line.ends_with('…'));
    }

    #[test]
    fn table_aligns_columns() {
        let out = plain(100).render_table(
            &["model", "account"],
            &[&["gpt-6", "work"], &["claude-opus-4", "personal"]],
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "model          account");
        assert_eq!(lines[1], "gpt-6          work");
        assert_eq!(lines[2], "claude-opus-4  personal");
        // No box drawing or trailing whitespace.
        assert!(!out.contains('|'));
        assert!(lines.iter().all(|l| l.trim_end() == *l));
    }

    #[test]
    fn table_fills_missing_cells() {
        let out = plain(100).render_table(&["a", "b"], &[&["1"]]);
        assert_eq!(out.lines().nth(1).unwrap(), "1");
        let aligned = plain(100).render_table(&["aa", "bb"], &[&["1"]]);
        assert_eq!(aligned.lines().nth(1).unwrap(), "1");
    }

    #[test]
    fn table_fits_width() {
        let cell = "x".repeat(60);
        let out = plain(40).render_table(&["first", "second"], &[&[&cell, &cell], &[&cell, &cell]]);
        for line in out.lines() {
            assert!(chars(line) <= 40, "line too wide: {line}");
        }
        assert!(out.contains('…'));
    }

    #[test]
    fn truncate_keeps_short_text() {
        assert_eq!(truncate("abc", 5, "…"), "abc");
        assert_eq!(truncate("abcdef", 4, "…"), "abc…");
        assert_eq!(truncate("abcdef", 4, "..."), "a...");
        assert_eq!(pad("abcdef", 1, "..."), "a");
        assert_eq!(pad("abcdef", 3, "..."), "abc");
    }

    #[test]
    fn mask_hides_secrets() {
        assert_eq!(mask("sk-1234567890abcd"), "sk-…abcd");
        assert_eq!(mask("abcdefgh"), "abc…efgh");
        assert_eq!(mask("short"), "…");
        assert_eq!(mask(""), "");
        assert!(chars(&mask("abcdefgh")) <= 8);
    }

    #[test]
    fn mask_never_shows_more_than_seven_chars() {
        for n in 1..40 {
            let secret = "a".repeat(n);
            let shown = chars(&mask(&secret).replace('…', ""));
            assert!(shown <= 7, "{n}: shown {shown}");
        }
    }

    #[test]
    fn color_requires_tty_and_clean_env() {
        assert!(colors_enabled(
            true,
            None,
            Some("xterm-256color"),
            None,
            None
        ));
        assert!(!colors_enabled(
            false,
            None,
            Some("xterm-256color"),
            None,
            None
        ));
        assert!(!colors_enabled(true, Some("1"), Some("xterm"), None, None));
        assert!(!colors_enabled(true, None, Some("dumb"), None, None));
        assert!(colors_enabled(
            true,
            Some("1"),
            Some("dumb"),
            Some("1"),
            None
        ));
        assert!(colors_enabled(
            false,
            Some("1"),
            Some("dumb"),
            None,
            Some("1")
        ));
        assert!(!colors_enabled(false, None, None, Some("0"), Some("0")));
    }

    #[test]
    fn unicode_follows_locale_and_override() {
        assert!(use_unicode(None, None, Some("en_US.UTF-8")));
        assert!(use_unicode(None, Some("C.utf8"), Some("C")));
        assert!(!use_unicode(None, None, Some("C")));
        assert!(!use_unicode(None, None, None));
        assert!(!use_unicode(Some("1"), None, Some("en_US.UTF-8")));
        assert!(use_unicode(Some("0"), None, Some("en_US.UTF-8")));
    }

    #[test]
    fn ascii_symbols_when_not_utf8() {
        let ascii = Ui::custom(false, false, 80);
        assert_eq!(ascii.sym().ok, "+");
        assert_eq!(ascii.sym().warn, "!");
        assert_eq!(ascii.sym().err, "x");
        assert_eq!(ascii.sym().info, "-");
    }

    #[test]
    fn colors_applied_only_when_enabled() {
        let styled = Ui::custom(true, true, 40).render_kv(&[("k", "v")]);
        assert!(styled.starts_with("\u{1b}["));
        let plain = Ui::custom(false, true, 40).render_kv(&[("k", "v")]);
        assert!(!plain.contains('\u{1b}'));
    }

    #[test]
    fn fit_shrinks_largest_first() {
        assert_eq!(fit(&[5, 5], 100), vec![5, 5]);
        let fitted = fit(&[60, 60], 40);
        assert!(fitted.iter().sum::<usize>() <= 40);
        assert!(fitted.iter().all(|w| *w >= 1));
    }
}
