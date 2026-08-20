/// Output formatting for search results.
///
/// Supports heading/flat/JSON/vimgrep formats, color control,
/// context lines, null separators, and trimming.
use std::io::{self, Write};
use std::time::Instant;

/// A single match result.
pub struct Match {
    pub file: String,
    pub line_number: usize,
    pub content: String,
    /// Column (1-based) of first match in content (for vimgrep).
    pub column: Option<usize>,
    /// Byte ranges of the matches inside `content`, used for highlighting,
    /// per-match `--vimgrep` rows, and JSON `submatches`.
    pub spans: Vec<(usize, usize)>,
    /// Byte offset of the start of `content` within the file (JSON only).
    pub absolute_offset: usize,
}

/// A context (non-matching) line surrounding a match.
pub struct ContextLine {
    pub file: String,
    pub line_number: usize,
    pub content: String,
    /// Byte offset of the start of `content` within the file (JSON only).
    pub absolute_offset: usize,
}

/// ANSI escape wrapping a matched substring: bold red, as ripgrep does.
const MATCH_COLOR: &str = "\x1b[1;31m";
const COLOR_RESET: &str = "\x1b[0m";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

impl ColorMode {
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
}

impl OutputConfig {
    /// Build config from CLI flags, auto-detecting format and color.
    #[allow(clippy::too_many_arguments)]
    pub fn from_flags(
        json: bool,
        files_only: bool,
        count: bool,
        vimgrep: bool,
        heading: Option<bool>,
        color: ColorMode,
        null: bool,
        trim: bool,
        no_filename: bool,
        no_line_number: bool,
        context: bool,
    ) -> Self {
        let format = if json {
            OutputFormat::Json
        } else if files_only {
            OutputFormat::FilesOnly
        } else if count {
            OutputFormat::Count
        } else if vimgrep {
            OutputFormat::Vimgrep
        } else {
            // Heading vs flat is resolved in the writer
            OutputFormat::Heading
        };
        Self {
            format,
            color,
            heading,
            null,
            trim,
            no_filename,
            no_line_number,
            context,
        }
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
            all_bytes_searched: 0,
            all_searches: 0,
        }
    }

    fn is_json(&self) -> bool {
        self.config.format == OutputFormat::Json
    }

    /// Record that a file of `bytes` length is about to be searched.
    pub fn note_bytes_searched(&mut self, bytes: u64) {
        self.pending_bytes_searched = bytes;
        self.all_bytes_searched += bytes;
        self.all_searches += 1;
    }

    /// Report that a matching file was suppressed because it looks binary.
    pub fn write_binary_note(&mut self, file: &str, offset: usize) -> io::Result<()> {
        if self.is_json() {
            self.ensure_json_begin(file)?;
            return Ok(());
        }
        writeln!(
            self.stdout,
            "Binary file {file} matches (found \"\\0\" byte around offset {offset})"
        )
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
            "data": { "path": { "text": file } },
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
        let msg = serde_json::json!({
            "type": "end",
            "data": {
                "path": { "text": file },
                "binary_offset": serde_json::Value::Null,
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
        if let Some((ref last_file, last_line)) = self.last_printed_line
            && last_file == file
            && line_num > last_line + 1
        {
            writeln!(self.stdout, "--")?;
        }
        Ok(())
    }

    /// Write a context (non-matching) line.
    pub fn write_context_line(&mut self, ctx: &ContextLine) -> io::Result<()> {
        if self.is_json() {
            self.ensure_json_begin(&ctx.file)?;
            let (content, _) = self.trim_adjust(&ctx.content, &[]);
            let msg = serde_json::json!({
                "type": "context",
                "data": {
                    "path": { "text": ctx.file },
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
        let content = content.to_string();
        if self.use_heading && !self.config.no_filename {
            if self.config.no_line_number {
                writeln!(self.stdout, "{content}")?;
            } else if self.use_color {
                writeln!(self.stdout, "\x1b[32m{}\x1b[0m-{content}", ctx.line_number)?;
            } else {
                writeln!(self.stdout, "{}-{content}", ctx.line_number)?;
            }
        } else {
            let show_file = !self.config.no_filename;
            let show_line = !self.config.no_line_number;
            match (show_file, show_line) {
                (true, true) => {
                    writeln!(self.stdout, "{}-{}-{content}", ctx.file, ctx.line_number)?
                }
                (true, false) => writeln!(self.stdout, "{}-{content}", ctx.file)?,
                (false, true) => writeln!(self.stdout, "{}-{content}", ctx.line_number)?,
                (false, false) => writeln!(self.stdout, "{content}")?,
            }
        }
        self.last_printed_line = Some((ctx.file.clone(), ctx.line_number));
        Ok(())
    }

    pub fn write_match(&mut self, m: &Match) -> io::Result<()> {
        let (content, spans) = self.trim_adjust(&m.content, &m.spans);
        let content = content.to_string();
        match self.config.format {
            OutputFormat::Heading | OutputFormat::Flat => {
                if !self.config.no_filename {
                    self.ensure_heading(&m.file)?;
                }
                let rendered = self.highlight(&content, &spans);
                if self.use_heading && !self.config.no_filename {
                    if self.config.no_line_number {
                        writeln!(self.stdout, "{rendered}")?;
                    } else if self.use_color {
                        writeln!(self.stdout, "\x1b[32m{}\x1b[0m:{rendered}", m.line_number)?;
                    } else {
                        writeln!(self.stdout, "{}:{rendered}", m.line_number)?;
                    }
                } else {
                    let show_file = !self.config.no_filename;
                    let show_line = !self.config.no_line_number;
                    match (show_file, show_line) {
                        (true, true) => {
                            writeln!(self.stdout, "{}:{}:{rendered}", m.file, m.line_number)?
                        }
                        (true, false) => writeln!(self.stdout, "{}:{rendered}", m.file)?,
                        (false, true) => writeln!(self.stdout, "{}:{rendered}", m.line_number)?,
                        (false, false) => writeln!(self.stdout, "{rendered}")?,
                    }
                }
            }
            OutputFormat::Vimgrep => {
                // ripgrep emits one row per match, not per matching line, so
                // editors can step through every hit.
                let rendered = self.highlight(&content, &spans);
                if spans.is_empty() {
                    let col = m.column.unwrap_or(1);
                    writeln!(self.stdout, "{}:{}:{col}:{rendered}", m.file, m.line_number)?;
                } else {
                    for &(start, _) in &spans {
                        writeln!(
                            self.stdout,
                            "{}:{}:{}:{rendered}",
                            m.file,
                            m.line_number,
                            start + 1
                        )?;
                    }
                }
            }
            OutputFormat::Json => {
                self.ensure_json_begin(&m.file)?;
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
                        "path": { "text": m.file },
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
        Ok(())
    }

    pub fn write_file(&mut self, path: &str) -> io::Result<()> {
        if self.config.null {
            write!(self.stdout, "{path}\0")?;
        } else {
            writeln!(self.stdout, "{path}")?;
        }
        Ok(())
    }

    pub fn write_count(&mut self, file: &str, count: usize) -> io::Result<()> {
        if self.config.no_filename {
            writeln!(self.stdout, "{count}")?;
        } else {
            writeln!(self.stdout, "{file}:{count}")?;
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
            if self.use_color {
                writeln!(self.stdout, "\x1b[35m{file}\x1b[0m")?;
            } else {
                writeln!(self.stdout, "{file}")?;
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
fn atty_check() -> bool {
    use std::io::IsTerminal;
    io::stdout().is_terminal()
}
