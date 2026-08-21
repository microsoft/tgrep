/// Output formatting for search results.
///
/// Supports heading/flat/JSON/vimgrep formats, color control,
/// context lines, null separators, and trimming.
use std::borrow::Cow;
use std::io::{self, Write};
use std::time::Instant;

/// A single match result.
pub struct Match {
    pub file: String,
    pub line_number: usize,
    pub content: String,
    /// 1-based byte columns of each match in `content`, in source-byte terms.
    pub columns: Vec<usize>,
    /// Byte ranges of the matches inside `content`, used for highlighting,
    /// per-match `--vimgrep` rows, and JSON `submatches`.
    pub spans: Vec<(usize, usize)>,
    /// Byte offset of the start of `content` within the file (JSON only).
    pub absolute_offset: usize,
    /// Bytes of line terminator stripped from `content`. `-M/--max-columns`
    /// measures the line including its terminator, as ripgrep does.
    pub terminator_len: usize,
}

impl Match {
    /// Column reported by `--column`, which is the first match on the line.
    fn column(&self) -> Option<usize> {
        self.columns.first().copied()
    }
}

/// A context (non-matching) line surrounding a match.
pub struct ContextLine {
    pub file: String,
    pub line_number: usize,
    pub content: String,
    /// Byte offset of the start of `content` within the file (JSON only).
    pub absolute_offset: usize,
    /// See [`Match::terminator_len`].
    pub terminator_len: usize,
}

/// ANSI escape wrapping a matched substring: bold red, as ripgrep does.
const MATCH_COLOR: &str = "\x1b[1;31m";
const COLOR_RESET: &str = "\x1b[0m";

#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(Default))]
pub enum ColorMode {
    #[cfg_attr(test, default)]
    Auto,
    Always,
    Never,
}

impl ColorMode {
    /// Whether highlighting will actually be emitted.
    ///
    /// Mirrors the decision [`OutputWriter::new`] makes, so callers can tell
    /// ahead of time whether match spans are going to be used for anything.
    pub fn is_enabled(self) -> bool {
        match self {
            ColorMode::Auto => atty_check(),
            ColorMode::Always => true,
            ColorMode::Never => false,
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Heading,
    Flat,
    Json,
    Vimgrep,
    FilesOnly,
    Count,
}

pub struct OutputConfig {
    pub format: OutputFormat,
    pub color: ColorMode,
    pub heading: Option<bool>,
    pub null: bool,
    pub trim: bool,
    pub no_filename: bool,
    pub no_line_number: bool,
    /// Whether `-A`/`-B`/`-C` asked for context lines.
    ///
    /// Gates the `--` separator: without context, a gap between two matching
    /// lines is the normal case, and ripgrep prints nothing between them.
    pub context: bool,
    /// `--column`: prefix each matching line with its first match's column.
    pub column: bool,
    /// `-b/--byte-offset`: prefix each line with its 0-based byte offset.
    pub byte_offset: bool,
    /// `-M/--max-columns`: suppress lines longer than this many bytes.
    pub max_columns: Option<usize>,
    /// `--max-columns-preview`: print a truncated preview instead of dropping
    /// an over-long line entirely.
    pub max_columns_preview: bool,
    /// `--context-separator` / `--no-context-separator`. `None` suppresses it.
    pub context_separator: Option<String>,
    /// `--field-match-separator`, between the fields of a matching line.
    pub field_match_separator: String,
    /// `--field-context-separator`, between the fields of a context line.
    pub field_context_separator: String,
    /// `--path-separator`, substituted for the platform separator in paths.
    pub path_separator: Option<String>,
    /// How to reconstruct the printed path from a search-root-relative one.
    pub path_display: PathDisplay,
    /// `--line-buffered`: flush after every line.
    pub line_buffered: bool,
}

/// How to render the path of a file found under a search argument.
///
/// ripgrep builds output paths by pushing onto the path the user typed, so the
/// argument survives verbatim into every printed path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PathDisplay {
    /// No path argument: print the path relative to the current directory.
    #[default]
    Bare,
    /// A directory argument, as typed and already terminated with a separator.
    /// The relative remainder is appended to it.
    Prefix(String),
    /// A single-file argument, as typed. Printed verbatim.
    Exact(String),
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            format: OutputFormat::Heading,
            color: ColorMode::Auto,
            heading: None,
            null: false,
            trim: false,
            no_filename: false,
            no_line_number: false,
            context: false,
            column: false,
            byte_offset: false,
            max_columns: None,
            max_columns_preview: false,
            context_separator: Some("--".to_string()),
            field_match_separator: ":".to_string(),
            field_context_separator: "-".to_string(),
            path_separator: None,
            path_display: PathDisplay::Bare,
            line_buffered: false,
        }
    }
}

impl OutputConfig {
    /// Whether output is a count of matching lines or files.
    #[allow(dead_code)]
    pub fn is_count(&self) -> bool {
        self.format == OutputFormat::Count
    }
}

/// Running totals for ripgrep-compatible JSON `stats` objects.
#[derive(Default, Clone, Copy)]
struct Stats {
    searches: u64,
    searches_with_match: u64,
    bytes_searched: u64,
    bytes_printed: u64,
    matched_lines: u64,
    matches: u64,
}

impl Stats {
    fn add(&mut self, other: &Stats) {
        self.searches += other.searches;
        self.searches_with_match += other.searches_with_match;
        self.bytes_searched += other.bytes_searched;
        self.bytes_printed += other.bytes_printed;
        self.matched_lines += other.matched_lines;
        self.matches += other.matches;
    }

    fn to_json(self, elapsed: std::time::Duration) -> serde_json::Value {
        serde_json::json!({
            "elapsed": duration_json(elapsed),
            "searches": self.searches,
            "searches_with_match": self.searches_with_match,
            "bytes_searched": self.bytes_searched,
            "bytes_printed": self.bytes_printed,
            "matched_lines": self.matched_lines,
            "matches": self.matches,
        })
    }
}

fn duration_json(d: std::time::Duration) -> serde_json::Value {
    serde_json::json!({
        "secs": d.as_secs(),
        "nanos": d.subsec_nanos(),
        "human": format!("{:.6}s", d.as_secs_f64()),
    })
}

/// Walk `i` back to the nearest UTF-8 character boundary at or below it.
///
/// Match offsets from the regex engine always land on boundaries, but clamping
/// them after `--trim` shifts content can land mid-character, and slicing there
/// would panic.
fn floor_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub struct OutputWriter {
    config: OutputConfig,
    stdout: io::BufWriter<io::Stdout>,
    current_file: Option<String>,
    use_color: bool,
    use_heading: bool,
    /// Track the last line we printed for context-gap detection.
    last_printed_line: Option<(String, usize)>,
    started: Instant,
    /// Stats for the file currently open in JSON mode.
    file_stats: Stats,
    total_stats: Stats,
    /// Size of the file currently being searched, for per-file JSON stats.
    pending_bytes_searched: u64,
    /// Offset of the first NUL byte in the file currently open in JSON mode.
    ///
    /// ripgrep reports this in the `end` message so consumers can tell a binary
    /// hit apart from a plain text one; the human-readable printer says
    /// "binary file matches" instead.
    current_binary_offset: Option<u64>,
    all_bytes_searched: u64,
    all_searches: u64,
}

impl OutputWriter {
    pub fn new(config: OutputConfig) -> Self {
        let is_tty = atty_check();
        let use_color = match config.color {
            ColorMode::Auto => is_tty,
            ColorMode::Always => true,
            ColorMode::Never => false,
        };
        let use_heading = match config.heading {
            Some(h) => h,
            None => {
                is_tty
                    && config.format != OutputFormat::Flat
                    && config.format != OutputFormat::Vimgrep
            }
        };
        Self {
            config,
            stdout: io::BufWriter::new(io::stdout()),
            current_file: None,
            use_color,
            use_heading,
            last_printed_line: None,
            started: Instant::now(),
            file_stats: Stats::default(),
            total_stats: Stats::default(),
            pending_bytes_searched: 0,
            current_binary_offset: None,
            all_bytes_searched: 0,
            all_searches: 0,
        }
    }

    pub fn is_json(&self) -> bool {
        self.config.format == OutputFormat::Json
    }

    /// Record that a file of `bytes` length is about to be searched.
    pub fn note_bytes_searched(&mut self, bytes: u64) {
        self.pending_bytes_searched = bytes;
        self.all_bytes_searched += bytes;
        self.all_searches += 1;
    }

    /// Report that a matching file was suppressed because it looks binary.
    ///
    /// ripgrep prefixes the note with the file name the same way it prefixes a
    /// match line, and omits it entirely when file names are suppressed.
    ///
    /// In JSON mode there is no note: ripgrep emits the matches as usual and
    /// records the offset on the `end` message instead, so record it here and
    /// let the caller go on to print the matches.
    pub fn write_binary_note(&mut self, file: &str, offset: usize) -> io::Result<()> {
        if self.is_json() {
            self.ensure_json_begin(file)?;
            self.current_binary_offset = Some(offset as u64);
            return Ok(());
        }
        let note = format!("binary file matches (found \"\\0\" byte around offset {offset})");
        if self.config.no_filename {
            writeln!(self.stdout, "{note}")
        } else {
            writeln!(self.stdout, "{}: {note}", self.display_path(file))
        }
    }

    /// Emit ripgrep's `begin` message when a new file starts producing output,
    /// closing the previous file's `end` message first.
    fn ensure_json_begin(&mut self, file: &str) -> io::Result<()> {
        if self.current_file.as_deref() == Some(file) {
            return Ok(());
        }
        self.finish_json_file()?;
        let msg = serde_json::json!({
            "type": "begin",
            "data": { "path": { "text": self.display_path(file) } },
        });
        let line = format!("{msg}\n");
        self.file_stats.searches = 1;
        self.file_stats.bytes_searched = self.pending_bytes_searched;
        self.file_stats.bytes_printed += line.len() as u64;
        self.stdout.write_all(line.as_bytes())?;
        self.current_file = Some(file.to_string());
        self.last_printed_line = None;
        Ok(())
    }

    fn finish_json_file(&mut self) -> io::Result<()> {
        let Some(file) = self.current_file.take() else {
            return Ok(());
        };
        if self.file_stats.matches > 0 {
            self.file_stats.searches_with_match = 1;
        }
        let binary_offset = match self.current_binary_offset.take() {
            Some(off) => serde_json::Value::from(off),
            None => serde_json::Value::Null,
        };
        let msg = serde_json::json!({
            "type": "end",
            "data": {
                "path": { "text": self.display_path(&file) },
                "binary_offset": binary_offset,
                "stats": self.file_stats.to_json(self.started.elapsed()),
            },
        });
        let line = format!("{msg}\n");
        self.file_stats.bytes_printed += line.len() as u64;
        self.stdout.write_all(line.as_bytes())?;
        self.total_stats.add(&self.file_stats);
        self.file_stats = Stats::default();
        Ok(())
    }

    /// Write a context separator (`--`) when there's a gap between printed lines.
    ///
    /// Only meaningful when context is being displayed. Without `-A`/`-B`/`-C`,
    /// non-adjacent matching lines are the normal case and ripgrep prints
    /// nothing between them; emitting `--` there would also corrupt
    /// `--vimgrep` quickfix parsing and `-o` pipelines.
    pub fn write_context_separator(&mut self, file: &str, line_num: usize) -> io::Result<()> {
        if self.is_json() || !self.config.context {
            return Ok(());
        }
        let Some(sep) = self.config.context_separator.clone() else {
            return Ok(());
        };
        if let Some((ref last_file, last_line)) = self.last_printed_line
            && last_file == file
            && line_num > last_line + 1
        {
            writeln!(self.stdout, "{sep}")?;
        }
        Ok(())
    }

    /// Render a path the way ripgrep does.
    ///
    /// ripgrep builds output paths by pushing onto the path the user typed, so
    /// the argument survives verbatim — `rg foo ./src` prints `./src\lib.rs` on
    /// Windows — while everything appended to it uses the platform separator.
    fn display_path(&self, file: &str) -> String {
        let joined = match &self.config.path_display {
            PathDisplay::Exact(p) => p.clone(),
            PathDisplay::Prefix(p) => format!("{p}{}", native_separators(file)),
            PathDisplay::Bare => native_separators(file),
        };
        match &self.config.path_separator {
            Some(sep) => joined.replace(['/', '\\'], sep),
            None => joined,
        }
    }

    /// Apply `-M/--max-columns`, replacing an over-long line with a note.
    ///
    /// ripgrep measures the line *including* its terminator, so an 80-byte line
    /// in an LF file is over a limit of 80 but the same text on a final,
    /// unterminated line is not. It also measures the *original* line, so a
    /// preview still reports how many bytes were dropped from the full line. A
    /// suppressed line is still printed, as a placeholder that names whether it
    /// was a match or context.
    fn apply_max_columns<'b>(
        &self,
        content: &'b str,
        terminator_len: usize,
        is_match: bool,
    ) -> Cow<'b, str> {
        let Some(limit) = self.config.max_columns else {
            return Cow::Borrowed(content);
        };
        if content.len() + terminator_len <= limit {
            return Cow::Borrowed(content);
        }
        if self.config.max_columns_preview {
            let cut = floor_boundary(content, limit);
            return Cow::Owned(format!(
                "{} [... omitted end of long line]",
                &content[..cut]
            ));
        }
        Cow::Borrowed(if is_match {
            "[Omitted long matching line]"
        } else {
            "[Omitted long context line]"
        })
    }

    /// The `file:line:col:offset:` prefix shared by match and context lines.
    fn field_prefix(
        &self,
        file: &str,
        line_number: usize,
        column: Option<usize>,
        offset: usize,
        is_match: bool,
    ) -> String {
        let sep = if is_match {
            &self.config.field_match_separator
        } else {
            &self.config.field_context_separator
        };
        let mut out = String::new();
        let heading_active = self.use_heading && !self.config.no_filename;
        if !heading_active && !self.config.no_filename {
            out.push_str(&self.display_path(file));
            out.push_str(sep);
        }
        if !self.config.no_line_number {
            if self.use_color {
                out.push_str(&format!("\x1b[32m{line_number}\x1b[0m"));
            } else {
                out.push_str(&line_number.to_string());
            }
            out.push_str(sep);
        }
        if self.config.column && is_match {
            out.push_str(&column.unwrap_or(1).to_string());
            out.push_str(sep);
        }
        if self.config.byte_offset {
            out.push_str(&offset.to_string());
            out.push_str(sep);
        }
        out
    }

    /// Write a context (non-matching) line.
    pub fn write_context_line(&mut self, ctx: &ContextLine) -> io::Result<()> {
        if self.is_json() {
            self.ensure_json_begin(&ctx.file)?;
            let (content, _) = self.trim_adjust(&ctx.content, &[]);
            let msg = serde_json::json!({
                "type": "context",
                "data": {
                    "path": { "text": self.display_path(&ctx.file) },
                    "lines": { "text": format!("{content}\n") },
                    "line_number": ctx.line_number,
                    "absolute_offset": ctx.absolute_offset,
                    "submatches": [],
                },
            });
            let line = format!("{msg}\n");
            self.file_stats.bytes_printed += line.len() as u64;
            self.stdout.write_all(line.as_bytes())?;
            self.last_printed_line = Some((ctx.file.clone(), ctx.line_number));
            return Ok(());
        }

        if !self.config.no_filename {
            self.ensure_heading(&ctx.file)?;
        }
        let (content, _) = self.trim_adjust(&ctx.content, &[]);
        let content = self.apply_max_columns(content, ctx.terminator_len, false);
        let prefix =
            self.field_prefix(&ctx.file, ctx.line_number, None, ctx.absolute_offset, false);
        writeln!(self.stdout, "{prefix}{content}")?;
        self.last_printed_line = Some((ctx.file.clone(), ctx.line_number));
        self.maybe_flush()
    }

    pub fn write_match(&mut self, m: &Match) -> io::Result<()> {
        let (content, spans) = self.trim_adjust(&m.content, &m.spans);
        let content = content.to_string();
        match self.config.format {
            OutputFormat::Heading | OutputFormat::Flat => {
                if !self.config.no_filename {
                    self.ensure_heading(&m.file)?;
                }
                let clipped = self.apply_max_columns(&content, m.terminator_len, true);
                // Highlighting only lines up when the text was not clipped.
                let rendered = if clipped.len() == content.len() {
                    self.highlight(&content, &spans)
                } else {
                    clipped.into_owned()
                };
                let prefix =
                    self.field_prefix(&m.file, m.line_number, m.column(), m.absolute_offset, true);
                writeln!(self.stdout, "{prefix}{rendered}")?;
            }
            OutputFormat::Vimgrep => {
                // ripgrep emits one row per match, not per matching line, so
                // editors can step through every hit.
                let rendered = self.highlight(&content, &spans);
                let file = self.display_path(&m.file);
                if m.columns.is_empty() {
                    writeln!(self.stdout, "{file}:{}:1:{rendered}", m.line_number)?;
                } else {
                    for col in &m.columns {
                        writeln!(self.stdout, "{file}:{}:{col}:{rendered}", m.line_number)?;
                    }
                }
            }
            OutputFormat::Json => {
                self.ensure_json_begin(&m.file)?;
                // `start`/`end` index `lines.text` as emitted, which is the
                // decoded text. ripgrep sidesteps the question by emitting
                // `lines.bytes` (base64 of the raw line) whenever a line is not
                // valid UTF-8, and then reporting source offsets. tgrep always
                // emits `text`, so its offsets stay usable for slicing it —
                // they can differ from `absolute_offset`, which is a real file
                // offset, on a line containing invalid UTF-8.
                let submatches: Vec<serde_json::Value> = spans
                    .iter()
                    .map(|&(s, e)| {
                        serde_json::json!({
                            "match": { "text": &content[s..e] },
                            "start": s,
                            "end": e,
                        })
                    })
                    .collect();
                self.file_stats.matched_lines += 1;
                self.file_stats.matches += submatches.len().max(1) as u64;
                let msg = serde_json::json!({
                    "type": "match",
                    "data": {
                        "path": { "text": self.display_path(&m.file) },
                        "lines": { "text": format!("{content}\n") },
                        "line_number": m.line_number,
                        "absolute_offset": m.absolute_offset,
                        "submatches": submatches,
                    },
                });
                let line = format!("{msg}\n");
                self.file_stats.bytes_printed += line.len() as u64;
                self.stdout.write_all(line.as_bytes())?;
            }
            OutputFormat::FilesOnly | OutputFormat::Count => {}
        }
        self.last_printed_line = Some((m.file.clone(), m.line_number));
        self.maybe_flush()
    }

    pub fn write_file(&mut self, path: &str) -> io::Result<()> {
        let path = self.display_path(path);
        if self.config.null {
            write!(self.stdout, "{path}\0")?;
        } else {
            writeln!(self.stdout, "{path}")?;
        }
        self.maybe_flush()
    }

    pub fn write_count(&mut self, file: &str, count: usize) -> io::Result<()> {
        if self.config.no_filename {
            writeln!(self.stdout, "{count}")?;
        } else {
            let file = self.display_path(file);
            let sep = &self.config.field_match_separator;
            writeln!(self.stdout, "{file}{sep}{count}")?;
        }
        self.maybe_flush()
    }

    /// Honour `--line-buffered` so a pipeline sees results as they are found.
    fn maybe_flush(&mut self) -> io::Result<()> {
        if self.config.line_buffered {
            self.stdout.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.is_json() {
            self.finish_json_file()?;
            let elapsed = self.started.elapsed();
            // Per-file `end` stats only cover files that produced output;
            // the summary covers everything that was searched.
            self.total_stats.searches = self.all_searches;
            self.total_stats.bytes_searched = self.all_bytes_searched;
            let msg = serde_json::json!({
                "type": "summary",
                "data": {
                    "elapsed_total": duration_json(elapsed),
                    "stats": self.total_stats.to_json(elapsed),
                },
            });
            writeln!(self.stdout, "{msg}")?;
        }
        self.stdout.flush()
    }

    /// Wrap each matched range in `content` with the match color.
    fn highlight(&self, content: &str, spans: &[(usize, usize)]) -> String {
        if !self.use_color || spans.is_empty() {
            return content.to_string();
        }
        let mut out = String::with_capacity(content.len() + spans.len() * 12);
        let mut pos = 0;
        for &(start, end) in spans {
            let start = floor_boundary(content, start).max(pos);
            let end = floor_boundary(content, end).max(start);
            if start > pos {
                out.push_str(&content[pos..start]);
            }
            if end > start {
                out.push_str(MATCH_COLOR);
                out.push_str(&content[start..end]);
                out.push_str(COLOR_RESET);
            }
            pos = end;
        }
        out.push_str(&content[pos..]);
        out
    }

    fn ensure_heading(&mut self, file: &str) -> io::Result<()> {
        if !self.use_heading {
            return Ok(());
        }
        if self.current_file.as_deref() != Some(file) {
            if self.current_file.is_some() {
                writeln!(self.stdout)?;
            }
            let shown = self.display_path(file);
            if self.use_color {
                writeln!(self.stdout, "\x1b[35m{shown}\x1b[0m")?;
            } else {
                writeln!(self.stdout, "{shown}")?;
            }
            self.current_file = Some(file.to_string());
            self.last_printed_line = None;
        }
        Ok(())
    }

    /// Apply `--trim`, shifting match spans to stay aligned with the content.
    fn trim_adjust<'a>(
        &self,
        s: &'a str,
        spans: &[(usize, usize)],
    ) -> (&'a str, Vec<(usize, usize)>) {
        if !self.config.trim {
            return (s, spans.to_vec());
        }
        let trimmed = s.trim();
        let delta = s.len() - s.trim_start().len();
        let adjusted = spans
            .iter()
            .filter_map(|&(a, b)| {
                let a = a.saturating_sub(delta).min(trimmed.len());
                let b = b.saturating_sub(delta).min(trimmed.len());
                (b > a).then_some((a, b))
            })
            .collect();
        (trimmed, adjusted)
    }
}

/// Simple TTY check using std::io::IsTerminal (stable since Rust 1.70).
pub(crate) fn atty_check() -> bool {
    use std::io::IsTerminal;
    io::stdout().is_terminal()
}

/// Convert the index's `/`-separated relative paths to the platform separator.
fn native_separators(path: &str) -> String {
    if std::path::MAIN_SEPARATOR == '/' {
        path.to_string()
    } else {
        path.replace('/', std::path::MAIN_SEPARATOR_STR)
    }
}
