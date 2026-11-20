//! Best-effort parsing of systemd unit files that were *not* written by
//! `mongoose setup`, so `mongoose show` can still tell what they do.
//!
//! Implements the parts of the systemd syntax that matter for finding the
//! rules: comments, line continuations, sections, `ExecStart=` (including
//! resets, prefixes and quoting) and command line arguments, which are then
//! interpreted with mongoose's own CLI parser. Anything that can only be
//! resolved by systemd at runtime (specifiers, environment variables) is
//! reported instead of guessed.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::cli::{Cli, Command};
use crate::spec::ForwardSpec;

/// Largest unit file that is read; anything bigger is not a plausible unit.
pub const MAX_UNIT_SIZE: u64 = 1024 * 1024;

/// What a foreign unit's `ExecStart=` runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUnit {
    pub exe: PathBuf,
    pub specs: Vec<ForwardSpec>,
    pub verbose: bool,
    /// Things worth knowing that do not prevent interpretation, e.g. prefixes.
    pub notes: Vec<String>,
}

/// Reads a unit file as text without ever blocking or reading unbounded
/// data: only regular files (after following symlinks) up to
/// [`MAX_UNIT_SIZE`] that are valid UTF-8.
pub fn read_unit_text(path: &Path) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|err| format!("cannot read: {err}"))?;
    if meta.is_dir() {
        return Err("is a directory".to_string());
    }
    if !meta.is_file() {
        return Err("is not a regular file".to_string());
    }
    let file = fs::File::open(path).map_err(|err| format!("cannot read: {err}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_UNIT_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("cannot read: {err}"))?;
    if bytes.len() as u64 > MAX_UNIT_SIZE {
        return Err(format!(
            "is larger than {} KiB; not a unit file",
            MAX_UNIT_SIZE / 1024
        ));
    }
    String::from_utf8(bytes).map_err(|_| "is not valid UTF-8 text".to_string())
}

/// Joins continuation lines and drops comments and blank lines.
fn logical_lines(contents: &str) -> Vec<String> {
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let mut lines = Vec::new();
    let mut current: Option<String> = None;
    for raw in contents.lines() {
        let line = raw.trim();
        // Comment lines are ignored, also in the middle of a continuation.
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let (text, continues) = match line.strip_suffix('\\') {
            Some(text) => (text, true),
            None => (line, false),
        };
        let joined = match current.take() {
            Some(mut previous) => {
                previous.push(' ');
                previous.push_str(text);
                previous
            }
            None => text.to_string(),
        };
        if continues {
            current = Some(joined);
        } else if !joined.trim().is_empty() {
            lines.push(joined);
        }
    }
    if let Some(rest) = current
        && !rest.trim().is_empty()
    {
        lines.push(rest);
    }
    lines
}

/// The effective `ExecStart=` values of all `[Service]` sections.
fn exec_start_values(contents: &str) -> Vec<String> {
    let mut in_service = false;
    let mut values = Vec::new();
    for line in logical_lines(contents) {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_service = line == "[Service]";
            continue;
        }
        if !in_service {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim() == "ExecStart"
        {
            let value = value.trim();
            if value.is_empty() {
                // An empty assignment resets the list.
                values.clear();
            } else {
                values.push(value.to_string());
            }
        }
    }
    values
}

fn unescape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<char, String> {
    let c = chars
        .next()
        .ok_or_else(|| "trailing backslash".to_string())?;
    Ok(match c {
        '\\' | '"' | '\'' => c,
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        's' => ' ',
        'a' => '\x07',
        'b' => '\x08',
        'f' => '\x0c',
        'v' => '\x0b',
        'x' => {
            let hex: String = (0..2).filter_map(|_| chars.next()).collect();
            u8::from_str_radix(&hex, 16)
                .ok()
                .filter(|_| hex.len() == 2)
                .map(char::from)
                .ok_or_else(|| format!("invalid escape \\x{hex}"))?
        }
        other => return Err(format!("unsupported escape \\{other}")),
    })
}

/// Splits a systemd command line into words: whitespace separated, with
/// single/double quotes anywhere in a word and C-style escapes. `%%` and
/// (when `expand_env`) `$$` are unescaped; any other specifier or variable
/// cannot be resolved outside systemd and is an error.
pub fn split_systemd_words(line: &str, expand_env: bool) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_ascii_whitespace()).is_some() {}
        if chars.peek().is_none() {
            return Ok(words);
        }
        let mut word = String::new();
        let mut had_quotes = false;
        let mut quote: Option<char> = None;
        while let Some(&c) = chars.peek() {
            if quote.is_none() && c.is_ascii_whitespace() {
                break;
            }
            chars.next();
            match c {
                '"' | '\'' if quote == Some(c) => quote = None,
                '"' | '\'' if quote.is_none() => {
                    quote = Some(c);
                    had_quotes = true;
                }
                '\\' => word.push(unescape(&mut chars)?),
                '%' => match chars.next() {
                    Some('%') => word.push('%'),
                    Some(spec) => {
                        return Err(format!(
                            "uses the systemd specifier %{spec}, which can only be resolved by systemd"
                        ));
                    }
                    None => return Err("trailing '%'".to_string()),
                },
                '$' if expand_env => match chars.next() {
                    Some('$') => word.push('$'),
                    _ => {
                        return Err(
                            "uses environment variable expansion, which can only be resolved by systemd"
                                .to_string(),
                        );
                    }
                },
                c => word.push(c),
            }
        }
        if quote.is_some() {
            return Err("unterminated quote".to_string());
        }
        if word == ";" && !had_quotes {
            return Err("runs several commands separated by ';'".to_string());
        }
        words.push(word);
    }
}

/// Interprets the `ExecStart=` of a unit not written by `mongoose setup`.
pub fn parse_foreign_unit(contents: &str) -> Result<ParsedUnit, String> {
    let values = exec_start_values(contents);
    let value = match values.as_slice() {
        [value] => value.as_str(),
        [] => return Err("no ExecStart= in [Service]".to_string()),
        _ => {
            return Err(format!(
                "{} ExecStart= commands (only possible for Type=oneshot); not a forwarder",
                values.len()
            ));
        }
    };

    let mut notes = Vec::new();
    let mut rest = value;
    let mut custom_argv0 = false;
    let mut expand_env = true;
    loop {
        rest = match rest.chars().next() {
            Some('-') => {
                notes.push("ExecStart '-' prefix: a failing exit status is ignored".to_string());
                &rest[1..]
            }
            Some('@') => {
                custom_argv0 = true;
                &rest[1..]
            }
            Some(':') => {
                expand_env = false;
                &rest[1..]
            }
            Some('+') => {
                notes.push("ExecStart '+' prefix: runs with full privileges".to_string());
                &rest[1..]
            }
            Some('!') => {
                notes.push("ExecStart '!' prefix: privilege changes are skipped".to_string());
                rest.trim_start_matches('!')
            }
            _ => break,
        };
    }

    let mut words = split_systemd_words(rest, expand_env)?;
    if words.is_empty() {
        return Err("ExecStart= has no command".to_string());
    }
    let exe = PathBuf::from(words.remove(0));
    if custom_argv0 {
        if words.is_empty() {
            return Err("ExecStart '@' prefix without argv[0]".to_string());
        }
        words.remove(0);
    }

    let cli = Cli::try_parse_args(std::iter::once("mongoose".to_string()).chain(words)).map_err(
        |err| {
            let text = err.to_string();
            let first = text.lines().next().unwrap_or("").trim();
            format!(
                "arguments are not a valid mongoose command line: {}",
                first.strip_prefix("error: ").unwrap_or(first)
            )
        },
    )?;
    let verbose = cli.verbose();
    match cli.command {
        Command::Start { specs, .. } => Ok(ParsedUnit {
            exe,
            specs,
            verbose,
            notes,
        }),
        Command::Setup { .. } => Err("ExecStart= runs `mongoose setup`, not `start`".to_string()),
        Command::Remove { .. } => Err("ExecStart= runs `mongoose remove`, not `start`".to_string()),
        Command::Show { .. } => Err("ExecStart= runs `mongoose show`, not `start`".to_string()),
    }
}
