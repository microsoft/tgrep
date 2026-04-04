/// Output formatting for search results.
///
/// Supports heading/flat/JSON/vimgrep formats, color control,
/// context lines, null separators, and trimming.
use std::io::{self, Write};

/// A single match result.
pub struct Match {
    pub file: String,
    pub line_number: usize,
    pub content: String,
    /// Column (1-based) of first match in content (for vimgrep).
    pub column: Option<usize>,
}

/// A context (non-matching) line surrounding a match.
pub struct ContextLine {
    pub file: String,
    pub line_number: usize,
    pub content: String,
}

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
        }
    }
}

pub struct OutputWriter {
    config: OutputConfig,
    stdout: io::BufWriter<io::Stdout>,
    current_file: Option<String>,
    use_color: bool,
    use_heading: bool,
    /// Track the last line we printed for context-gap detection.
    last_printed_line: Option<(String, usize)>,
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
        }
    }

    /// Write a context separator (`--`) when there's a gap between printed lines.
    pub fn write_context_separator(&mut self, file: &str, line_num: usize) -> io::Result<()> {
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
        match self.config.format {
            OutputFormat::Json => {
                let json = serde_json::json!({
                    "type": "context",
                    "file": ctx.file,
                    "line": ctx.line_number,
                    "content": self.maybe_trim(&ctx.content),
                });
                writeln!(self.stdout, "{json}")?;
            }
            _ => {
                if !self.config.no_filename {
                    self.ensure_heading(&ctx.file)?;
                }
                let content = self.maybe_trim(&ctx.content);
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
            }
        }
        self.last_printed_line = Some((ctx.file.clone(), ctx.line_number));
        Ok(())
    }

    pub fn write_match(&mut self, m: &Match) -> io::Result<()> {
        let content = self.maybe_trim(&m.content);
        match self.config.format {
            OutputFormat::Heading | OutputFormat::Flat => {
                if !self.config.no_filename {
                    self.ensure_heading(&m.file)?;
                }
                if self.use_heading && !self.config.no_filename {
                    if self.config.no_line_number {
                        writeln!(self.stdout, "{content}")?;
                    } else if self.use_color {
                        writeln!(self.stdout, "\x1b[32m{}\x1b[0m:{content}", m.line_number)?;
                    } else {
                        writeln!(self.stdout, "{}:{content}", m.line_number)?;
                    }
                } else {
                    let show_file = !self.config.no_filename;
                    let show_line = !self.config.no_line_number;
                    match (show_file, show_line) {
                        (true, true) => {
                            writeln!(self.stdout, "{}:{}:{content}", m.file, m.line_number)?
                        }
                        (true, false) => writeln!(self.stdout, "{}:{content}", m.file)?,
                        (false, true) => writeln!(self.stdout, "{}:{content}", m.line_number)?,
                        (false, false) => writeln!(self.stdout, "{content}")?,
                    }
                }
            }
            OutputFormat::Vimgrep => {
                let col = m.column.unwrap_or(1);
                writeln!(self.stdout, "{}:{}:{col}:{content}", m.file, m.line_number)?;
            }
            OutputFormat::Json => {
                let mut obj = serde_json::json!({
                    "type": "match",
                    "file": m.file,
                    "line": m.line_number,
                    "content": content,
                });
                if let Some(col) = m.column {
                    obj["column"] = serde_json::json!(col);
                }
                writeln!(self.stdout, "{obj}")?;
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
        self.stdout.flush()
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

    fn maybe_trim<'a>(&self, s: &'a str) -> &'a str {
        if self.config.trim { s.trim() } else { s }
    }
}

/// Simple TTY check using std::io::IsTerminal (stable since Rust 1.70).
fn atty_check() -> bool {
    use std::io::IsTerminal;
    io::stdout().is_terminal()
}
