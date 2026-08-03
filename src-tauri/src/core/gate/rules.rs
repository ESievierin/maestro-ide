//! Built-in gate rules and the shell-command parsing they share.
//!
//! The rules match on the `Bash` tool's `command` string. Parsing is a small
//! shell-aware tokenizer — single/double quotes, backslash escapes, command
//! substitution `$(…)` with here-doc awareness — not full POSIX, but enough for
//! the command shapes the agent CLI produces. Every token records its byte span
//! in the original string so `apply` can splice edited values back in without
//! disturbing the rest of the command.

use serde_json::Value;

use super::{GateMatch, GateParam, GateRule};

/// Tool the built-in rules match on.
const BASH_TOOL: &str = "Bash";

/// One shell token with its byte span in the original command string.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Token {
    /// Token text with quoting resolved.
    pub text: String,
    /// Command separator (`&&`, `||`, `;`, `|`, `&`; newlines normalize to `;`).
    pub is_separator: bool,
    /// Byte range of the raw token (including quotes) in the original command.
    pub start: usize,
    pub end: usize,
}

/// Shell-aware tokenizer. Quoted text keeps its literal content; command
/// substitutions are kept verbatim (including the `$(` … `)`), so a here-doc
/// commit message stays one token that [`unwrap_heredoc`] can pick apart.
pub(crate) fn tokenize(command: &str) -> Vec<Token> {
    let chars: Vec<(usize, char)> = command.char_indices().collect();
    let end_of = |i: usize| chars.get(i).map_or(command.len(), |&(o, _)| o);

    let mut tokens: Vec<Token> = Vec::new();
    let mut text = String::new();
    let mut start = 0;
    let mut has_token = false;
    let mut i = 0;

    macro_rules! flush {
        ($end:expr) => {
            if has_token {
                tokens.push(Token {
                    text: std::mem::take(&mut text),
                    is_separator: false,
                    start,
                    end: $end,
                });
                has_token = false;
            }
        };
    }

    while i < chars.len() {
        let (off, c) = chars[i];
        match c {
            ' ' | '\t' | '\r' => {
                flush!(off);
                i += 1;
            }
            '\n' | ';' => {
                flush!(off);
                tokens.push(Token {
                    text: ";".into(),
                    is_separator: true,
                    start: off,
                    end: end_of(i + 1),
                });
                i += 1;
            }
            '(' | ')' => {
                // Grouping punctuation: its own token so `(git push)` still parses as
                // a git invocation. `$(` never reaches here — it is consumed above.
                flush!(off);
                tokens.push(Token {
                    text: c.to_string(),
                    is_separator: true,
                    start: off,
                    end: end_of(i + 1),
                });
                i += 1;
            }
            '&' | '|' => {
                flush!(off);
                let doubled = chars.get(i + 1).is_some_and(|&(_, c2)| c2 == c);
                let width = if doubled { 2 } else { 1 };
                let op = if doubled {
                    format!("{c}{c}")
                } else {
                    c.to_string()
                };
                tokens.push(Token {
                    text: op,
                    is_separator: true,
                    start: off,
                    end: end_of(i + width),
                });
                i += width;
            }
            '\'' => {
                if !has_token {
                    has_token = true;
                    start = off;
                }
                i += 1;
                while i < chars.len() && chars[i].1 != '\'' {
                    text.push(chars[i].1);
                    i += 1;
                }
                i += 1; // closing quote (or past the end of unterminated input)
            }
            '"' => {
                if !has_token {
                    has_token = true;
                    start = off;
                }
                i += 1;
                while i < chars.len() {
                    let c2 = chars[i].1;
                    match c2 {
                        '"' => {
                            i += 1;
                            break;
                        }
                        '\\' => match chars.get(i + 1) {
                            Some(&(_, next)) if matches!(next, '"' | '\\' | '$' | '`') => {
                                text.push(next);
                                i += 2;
                            }
                            Some(&(_, next)) => {
                                text.push('\\');
                                text.push(next);
                                i += 2;
                            }
                            None => {
                                text.push('\\');
                                i += 1;
                            }
                        },
                        '$' if chars.get(i + 1).is_some_and(|&(_, n)| n == '(') => {
                            i = consume_substitution(&chars, i, &mut text);
                        }
                        _ => {
                            text.push(c2);
                            i += 1;
                        }
                    }
                }
            }
            '\\' => {
                // Backslash-newline is a line continuation: both characters vanish and
                // the token continues. Emitting the newline instead used to fuse the
                // next word into this token (`push\n--force`), which made every
                // subcommand match fail — the gate silently opened.
                if chars.get(i + 1).is_some_and(|&(_, n)| n == '\n') {
                    i += 2;
                    continue;
                }
                if chars.get(i + 1).is_some_and(|&(_, n)| n == '\r')
                    && chars.get(i + 2).is_some_and(|&(_, n)| n == '\n')
                {
                    i += 3;
                    continue;
                }
                if !has_token {
                    has_token = true;
                    start = off;
                }
                match chars.get(i + 1) {
                    Some(&(_, next)) => {
                        text.push(next);
                        i += 2;
                    }
                    None => i += 1,
                }
            }
            '$' if chars.get(i + 1).is_some_and(|&(_, n)| n == '(') => {
                if !has_token {
                    has_token = true;
                    start = off;
                }
                i = consume_substitution(&chars, i, &mut text);
            }
            _ => {
                if !has_token {
                    has_token = true;
                    start = off;
                }
                text.push(c);
                i += 1;
            }
        }
    }
    if has_token {
        tokens.push(Token {
            text,
            is_separator: false,
            start,
            end: command.len(),
        });
    }
    tokens
}

/// Consume `$(…)` starting at `i` (pointing at `$`), appending the raw text to
/// `out` verbatim. Tracks nested parens, quotes, and here-docs so a `)` inside
/// them does not close the substitution. Returns the index after the closing `)`.
fn consume_substitution(chars: &[(usize, char)], mut i: usize, out: &mut String) -> usize {
    out.push_str("$(");
    i += 2;
    let mut depth = 1u32;
    while i < chars.len() && depth > 0 {
        let c = chars[i].1;
        match c {
            '(' => {
                depth += 1;
                out.push(c);
                i += 1;
            }
            ')' => {
                depth -= 1;
                out.push(c);
                i += 1;
            }
            '\'' | '"' => {
                out.push(c);
                i += 1;
                while i < chars.len() && chars[i].1 != c {
                    if c == '"' && chars[i].1 == '\\' && i + 1 < chars.len() {
                        out.push('\\');
                        out.push(chars[i + 1].1);
                        i += 2;
                        continue;
                    }
                    out.push(chars[i].1);
                    i += 1;
                }
                if i < chars.len() {
                    out.push(c);
                    i += 1;
                }
            }
            '<' if chars.get(i + 1).is_some_and(|&(_, n)| n == '<') => {
                i = consume_heredoc(chars, i, out);
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    i
}

/// Consume `<<TAG` (also `<<'TAG'`, `<<"TAG"`, `<<-TAG`) plus everything through
/// the line consisting of TAG, appending verbatim. Returns the index after the
/// terminator line.
fn consume_heredoc(chars: &[(usize, char)], mut i: usize, out: &mut String) -> usize {
    out.push_str("<<");
    i += 2;
    if chars.get(i).is_some_and(|&(_, c)| c == '-') {
        out.push('-');
        i += 1;
    }
    while chars.get(i).is_some_and(|&(_, c)| c == ' ') {
        out.push(' ');
        i += 1;
    }
    let quote = match chars.get(i) {
        Some(&(_, c)) if c == '\'' || c == '"' => {
            out.push(c);
            i += 1;
            Some(c)
        }
        _ => None,
    };
    let mut tag = String::new();
    while i < chars.len() {
        let c = chars[i].1;
        let tag_ended = match quote {
            Some(q) => c == q,
            None => !(c.is_alphanumeric() || c == '_'),
        };
        if tag_ended {
            break;
        }
        tag.push(c);
        out.push(c);
        i += 1;
    }
    if let Some(q) = quote {
        if chars.get(i).is_some_and(|&(_, c)| c == q) {
            out.push(q);
            i += 1;
        }
    }
    // Rest of the line that introduced the here-doc.
    while i < chars.len() && chars[i].1 != '\n' {
        out.push(chars[i].1);
        i += 1;
    }
    // Body lines, through the terminator.
    while i < chars.len() {
        out.push('\n');
        i += 1;
        let line_start = i;
        while i < chars.len() && chars[i].1 != '\n' {
            i += 1;
        }
        let line: String = chars[line_start..i].iter().map(|&(_, c)| c).collect();
        out.push_str(&line);
        if line == tag {
            break;
        }
    }
    i
}

/// Command segments: token runs between separators.
pub(crate) fn split_commands(tokens: &[Token]) -> Vec<&[Token]> {
    tokens
        .split(|t| t.is_separator)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Quote a value for safe use in a POSIX shell command.
pub(crate) fn sh_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c));
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// If `value` is a `$(cat <<'TAG' … TAG)` substitution (the agent CLI's usual way
/// of passing multi-line commit messages and PR bodies), return the body.
pub(crate) fn unwrap_heredoc(value: &str) -> Option<String> {
    let inner = value.strip_prefix("$(")?.strip_suffix(')')?.trim();
    let rest = inner.strip_prefix("cat")?.trim_start();
    let rest = rest.strip_prefix("<<")?;
    let rest = rest.strip_prefix('-').unwrap_or(rest).trim_start();
    let (tag, after_tag) = if let Some(r) = rest.strip_prefix('\'') {
        let end = r.find('\'')?;
        (&r[..end], &r[end + 1..])
    } else if let Some(r) = rest.strip_prefix('"') {
        let end = r.find('"')?;
        (&r[..end], &r[end + 1..])
    } else {
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        (&rest[..end], &rest[end..])
    };
    if tag.is_empty() {
        return None;
    }
    let body = &after_tag[after_tag.find('\n')? + 1..];
    let terminator = format!("\n{tag}");
    let end = body.rfind(&terminator)?;
    if !body[end + terminator.len()..].trim().is_empty() {
        return None;
    }
    Some(body[..end].to_string())
}

/// Leading `VAR=value` environment assignments before the program name.
fn looks_like_assignment(text: &str) -> bool {
    match text.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// Words that merely prefix another command; the real program follows them.
const WRAPPER_WORDS: &[&str] = &[
    "command", "env", "time", "nohup", "exec", "sudo", "stdbuf", "then", "else", "do", "elif",
];

/// Grouping/no-op tokens that can precede a command inside a segment.
const GROUPING_TOKENS: &[&str] = &["(", ")", "{", "}", "!"];

/// Shells whose `-c <string>` argument is itself a command line.
const SHELL_PROGRAMS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];

/// Program name without its directory (`/usr/bin/git` → `git`).
pub(crate) fn program_basename(text: &str) -> &str {
    text.rsplit(['/', '\\']).next().unwrap_or(text)
}

/// Index of the program name in a segment, skipping `VAR=value` prefixes, grouping
/// tokens and wrapper words (`env`, `command`, `then`, …). Fail-closed: the point is
/// that `command git push` and `(git push)` still reach the gate.
fn program_index(segment: &[Token]) -> Option<usize> {
    segment.iter().position(|t| {
        let text = t.text.as_str();
        !looks_like_assignment(text)
            && !GROUPING_TOKENS.contains(&text)
            && !WRAPPER_WORDS.contains(&program_basename(text))
    })
}

/// Nested command strings inside `segment`: `sh -c "<cmd>"`, `eval "<cmd>"`, and any
/// `$(…)` / backtick token. Used to look for gated commands one level down; matches
/// found there are never editable (splicing into a nested quoted string is unsafe).
pub(crate) fn nested_commands(segment: &[Token]) -> Vec<String> {
    let mut out = Vec::new();
    let Some(program) = program_index(segment) else {
        return out;
    };
    let program_name = program_basename(&segment[program].text);

    if SHELL_PROGRAMS.contains(&program_name) {
        let mut i = program + 1;
        while i < segment.len() {
            if segment[i].text == "-c" {
                if let Some(script) = segment.get(i + 1) {
                    out.push(script.text.clone());
                }
                break;
            }
            i += 1;
        }
    } else if program_name == "eval" {
        for token in &segment[program + 1..] {
            out.push(token.text.clone());
        }
    }

    // Substitutions are kept verbatim by the tokenizer, e.g. `$(git push)`.
    for token in segment {
        let text = token.text.trim();
        if let Some(inner) = text
            .strip_prefix("$(")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            out.push(inner.to_string());
        } else if let Some(inner) = text.strip_prefix('`').and_then(|r| r.strip_suffix('`')) {
            out.push(inner.to_string());
        }
    }
    out
}

/// Git global flags that take a separate value argument.
const GIT_VALUE_FLAGS: &[&str] = &["-C", "-c", "--git-dir", "--work-tree", "--namespace"];

/// Index of the git subcommand token, skipping global flags (`git -C x push`).
fn git_subcommand(segment: &[Token]) -> Option<usize> {
    let program = program_index(segment)?;
    if program_basename(&segment[program].text) != "git" {
        return None;
    }
    let mut i = program + 1;
    while i < segment.len() {
        let text = segment[i].text.as_str();
        if !text.starts_with('-') {
            return Some(i);
        }
        i += if GIT_VALUE_FLAGS.contains(&text) {
            2
        } else {
            1
        };
    }
    None
}

fn is_git_push(segment: &[Token]) -> bool {
    git_subcommand(segment).is_some_and(|i| segment[i].text == "push")
}

fn is_git_commit(segment: &[Token]) -> bool {
    git_subcommand(segment).is_some_and(|i| segment[i].text == "commit")
}

fn is_gh_pr_create(segment: &[Token]) -> bool {
    let Some(program) = program_index(segment) else {
        return false;
    };
    if program_basename(&segment[program].text) != "gh" {
        return false;
    }
    let mut words = segment[program + 1..]
        .iter()
        .map(|t| t.text.as_str())
        .filter(|t| !t.starts_with('-'));
    words.next() == Some("pr") && words.next() == Some("create")
}

/// Where a flag's value lives, for span-based replacement.
struct FlagHit {
    value: String,
    /// Byte range in the original command to replace.
    start: usize,
    end: usize,
    /// Kept in front of the re-quoted value (`--title=` / `-m`); empty when the
    /// value is its own token.
    prefix: String,
}

/// Find `long`/`short` in the segment. Supports `--flag value`, `--flag=value`,
/// `-f value`, `-f=value`, `-fvalue`, and (with `cluster`) combined short flags
/// like `git commit -am "msg"`.
fn find_flag(segment: &[Token], long: &str, short: Option<&str>, cluster: bool) -> Option<FlagHit> {
    count_flag(segment, long, short, cluster).1
}

/// How many times the flag occurs, plus the hit git/gh would actually use (the last
/// one). Multiplicity makes the gate drop editing: showing one value while another
/// executes is exactly what this boundary exists to prevent.
fn count_flag(
    segment: &[Token],
    long: &str,
    short: Option<&str>,
    cluster: bool,
) -> (usize, Option<FlagHit>) {
    let (count, hit, _) = count_flag_detail(segment, long, short, cluster);
    (count, hit)
}

/// `(occurrences, last hit, ambiguous)`. Ambiguous means a flag's value looks like
/// another flag (`--title --body B`): the tool would swallow the flag as the value, so
/// the gate must not present it as editable text.
fn count_flag_detail(
    segment: &[Token],
    long: &str,
    short: Option<&str>,
    cluster: bool,
) -> (usize, Option<FlagHit>, bool) {
    let mut count = 0usize;
    let mut ambiguous = false;
    let mut last: Option<FlagHit> = None;
    for (i, token) in segment.iter().enumerate() {
        let text = token.text.as_str();
        if text == long || short == Some(text) {
            // A dangling flag has no value; keep scanning instead of giving up.
            if let Some(value) = segment.get(i + 1) {
                if value.text.starts_with('-') {
                    ambiguous = true;
                }
                count += 1;
                last = Some(FlagHit {
                    value: value.text.clone(),
                    start: value.start,
                    end: value.end,
                    prefix: String::new(),
                });
            }
            continue;
        }
        if let Some(rest) = text.strip_prefix(long) {
            if let Some(v) = rest.strip_prefix('=') {
                count += 1;
                last = Some(FlagHit {
                    value: v.to_string(),
                    start: token.start,
                    end: token.end,
                    prefix: format!("{long}="),
                });
                continue;
            }
        }
        if let Some(s) = short {
            if let Some(rest) = text.strip_prefix(s) {
                if !rest.is_empty() {
                    let (value, prefix) = match rest.strip_prefix('=') {
                        Some(v) => (v, format!("{s}=")),
                        None => (rest, s.to_string()),
                    };
                    count += 1;
                    last = Some(FlagHit {
                        value: value.to_string(),
                        start: token.start,
                        end: token.end,
                        prefix,
                    });
                    continue;
                }
            }
            let letter = s.trim_start_matches('-');
            if cluster
                && text.len() > 2
                && text.starts_with('-')
                && !text.starts_with("--")
                && text.ends_with(letter)
                && text[1..].chars().all(|c| c.is_ascii_alphanumeric())
            {
                if let Some(value) = segment.get(i + 1) {
                    count += 1;
                    last = Some(FlagHit {
                        value: value.text.clone(),
                        start: value.start,
                        end: value.end,
                        prefix: String::new(),
                    });
                }
                continue;
            }
        }
    }
    (count, last, ambiguous)
}

/// Extracted flag value with here-doc substitutions unwrapped to their body.
fn flag_value(segment: &[Token], long: &str, short: Option<&str>, cluster: bool) -> String {
    find_flag(segment, long, short, cluster)
        .map(|hit| unwrap_heredoc(&hit.value).unwrap_or(hit.value))
        .unwrap_or_default()
}

/// Span edit that puts `new_value` (re-quoted) into the flag's place; if the flag
/// is absent and the value non-empty, append `long value` at the segment's end.
fn flag_edit(
    segment: &[Token],
    long: &str,
    short: Option<&str>,
    cluster: bool,
    new_value: &str,
) -> Option<(usize, usize, String)> {
    match find_flag(segment, long, short, cluster) {
        Some(hit) => Some((
            hit.start,
            hit.end,
            format!("{}{}", hit.prefix, sh_quote(new_value)),
        )),
        None if new_value.is_empty() => None,
        None => {
            let end = segment.last()?.end;
            Some((end, end, format!(" {long} {}", sh_quote(new_value))))
        }
    }
}

/// Apply span edits right-to-left so earlier spans stay valid.
fn splice(command: &str, mut edits: Vec<(usize, usize, String)>) -> String {
    edits.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    let mut out = command.to_string();
    for (start, end, replacement) in edits {
        out.replace_range(start..end, &replacement);
    }
    out
}

fn command_of(args: &Value) -> Option<&str> {
    args.get("command")?.as_str()
}

fn bash_command<'a>(tool: &str, args: &'a Value) -> Option<&'a str> {
    if tool != BASH_TOOL {
        return None;
    }
    command_of(args)
}

/// Clone `args` with the `command` string replaced.
fn with_command(args: &Value, command: String) -> Value {
    let mut updated = args.clone();
    if let Some(obj) = updated.as_object_mut() {
        obj.insert("command".into(), Value::String(command));
    }
    updated
}

fn param(key: &str, label: &str, value: String, multiline: bool) -> GateParam {
    GateParam {
        key: key.into(),
        label: label.into(),
        value,
        multiline,
    }
}

fn edited_value<'a>(edited: &'a [GateParam], key: &str) -> Option<&'a str> {
    edited
        .iter()
        .find(|p| p.key == key)
        .map(|p| p.value.as_str())
}

/// Recursion depth for nested command strings (`sh -c "sh -c '…'"`).
const MAX_NESTING: usize = 3;

/// Does any segment of `command` — including one level down inside `sh -c`, `eval`,
/// `$(…)` or backticks — satisfy `pred`? Fail-closed: a wrapped `git push` must still
/// reach the gate even though its params cannot be edited safely.
fn any_segment_nested(command: &str, depth: usize, pred: &dyn Fn(&[Token]) -> bool) -> bool {
    let tokens = tokenize(command);
    let segments = split_commands(&tokens);
    if segments.iter().any(|seg| pred(seg)) {
        return true;
    }
    if depth == 0 {
        return false;
    }
    segments
        .iter()
        .flat_map(|seg| nested_commands(seg))
        .any(|inner| any_segment_nested(&inner, depth - 1, pred))
}

/// Segments of the top level only (where splicing is safe) matching `pred`.
fn top_level_segments<'a>(
    tokens: &'a [Token],
    pred: &dyn Fn(&[Token]) -> bool,
) -> Vec<&'a [Token]> {
    split_commands(tokens)
        .into_iter()
        .filter(|seg| pred(seg))
        .collect()
}

/// Flags that make the message/body come from somewhere the dialog cannot show.
const EXTERNAL_MESSAGE_FLAGS: &[&str] = &[
    "-F",
    "--file",
    "--body-file",
    "-C",
    "--reuse-message",
    "--fill",
    "--fill-first",
    "--fill-verbose",
    "--template",
];

/// `Some(reason)` when a segment sources its text externally, so the value shown in
/// the dialog would be empty and editing it would produce a broken command.
fn external_source(segment: &[Token]) -> Option<String> {
    for token in segment {
        let text = token.text.as_str();
        let flag = text.split('=').next().unwrap_or(text);
        if EXTERNAL_MESSAGE_FLAGS.contains(&flag) {
            return Some(format!(
                "the text comes from `{flag}` — approve the command as-is or deny it"
            ));
        }
    }
    None
}

// ---------- built-in rules ----------

/// Gates any `git push` invocation (also `git -C <path> push`, and pushes hidden
/// in compound commands). No editable params — the dialog shows the raw command.
pub struct GitPushRule;

impl GateRule for GitPushRule {
    fn id(&self) -> &str {
        "git_push"
    }

    fn matches(&self, tool: &str, args: &Value) -> Option<GateMatch> {
        let command = bash_command(tool, args)?;
        any_segment_nested(command, MAX_NESTING, &is_git_push).then(|| GateMatch {
            kind: "git push".into(),
            params: Vec::new(),
            note: None,
        })
    }

    fn apply(&self, args: &Value, _edited: &[GateParam]) -> Value {
        args.clone()
    }
}

/// Gates `gh pr create`; title and body are editable.
pub struct GhPrCreateRule;

const PARAM_TITLE: &str = "title";
const PARAM_BODY: &str = "body";

impl GateRule for GhPrCreateRule {
    fn id(&self) -> &str {
        "gh_pr_create"
    }

    fn matches(&self, tool: &str, args: &Value) -> Option<GateMatch> {
        let command = bash_command(tool, args)?;
        let tokens = tokenize(command);
        let segments = top_level_segments(&tokens, &is_gh_pr_create);

        // Wrapped/nested invocation: gate it, but never pretend it is editable.
        if segments.is_empty() {
            return any_segment_nested(command, MAX_NESTING, &is_gh_pr_create).then(|| {
                GateMatch {
                    kind: "PR creation".into(),
                    params: Vec::new(),
                    note: Some(
                        "the `gh pr create` call is nested inside another command —                          approve the command as-is or deny it"
                            .into(),
                    ),
                }
            });
        }
        if segments.len() > 1 {
            return Some(GateMatch {
                kind: "PR creation".into(),
                params: Vec::new(),
                note: Some(format!(
                    "{} `gh pr create` calls in one command — approve as-is or deny",
                    segments.len()
                )),
            });
        }

        let segment = segments[0];
        if let Some(reason) = external_source(segment) {
            return Some(GateMatch {
                kind: "PR creation".into(),
                params: Vec::new(),
                note: Some(reason),
            });
        }
        // A repeated flag means gh would use a value other than the one shown; an
        // ambiguous one (`--title --body B`) means gh swallows the next flag as text.
        let (title_count, _, title_ambiguous) =
            count_flag_detail(segment, "--title", Some("-t"), false);
        let (body_count, _, body_ambiguous) =
            count_flag_detail(segment, "--body", Some("-b"), false);
        if title_count > 1 || body_count > 1 {
            return Some(GateMatch {
                kind: "PR creation".into(),
                params: Vec::new(),
                note: Some(
                    "the title or body flag appears more than once — approve as-is or deny".into(),
                ),
            });
        }
        if title_ambiguous || body_ambiguous {
            return Some(GateMatch {
                kind: "PR creation".into(),
                params: Vec::new(),
                note: Some(
                    "a title/body flag is followed by another flag, so gh would use that as the text — approve as-is or deny"
                        .into(),
                ),
            });
        }

        Some(GateMatch {
            kind: "PR creation".into(),
            params: vec![
                param(
                    PARAM_TITLE,
                    "Title",
                    flag_value(segment, "--title", Some("-t"), false),
                    false,
                ),
                param(
                    PARAM_BODY,
                    "Body",
                    flag_value(segment, "--body", Some("-b"), false),
                    true,
                ),
            ],
            note: None,
        })
    }

    fn apply(&self, args: &Value, edited: &[GateParam]) -> Value {
        let Some(command) = command_of(args) else {
            return args.clone();
        };
        let tokens = tokenize(command);
        let segments = split_commands(&tokens);
        let Some(segment) = segments.into_iter().find(|s| is_gh_pr_create(s)) else {
            return args.clone();
        };
        let mut edits = Vec::new();
        for (key, long, short) in [(PARAM_TITLE, "--title", "-t"), (PARAM_BODY, "--body", "-b")] {
            if let Some(value) = edited_value(edited, key) {
                if let Some(edit) = flag_edit(segment, long, Some(short), false, value) {
                    edits.push(edit);
                }
            }
        }
        with_command(args, splice(command, edits))
    }
}

/// Gates `git commit`; the message is editable. Registered only when the
/// `gate_commit` setting is `"true"`.
pub struct GitCommitRule;

const PARAM_MESSAGE: &str = "message";

impl GateRule for GitCommitRule {
    fn id(&self) -> &str {
        "git_commit"
    }

    fn matches(&self, tool: &str, args: &Value) -> Option<GateMatch> {
        let command = bash_command(tool, args)?;
        let tokens = tokenize(command);
        let segments = top_level_segments(&tokens, &is_git_commit);

        if segments.is_empty() {
            return any_segment_nested(command, MAX_NESTING, &is_git_commit).then(|| GateMatch {
                kind: "commit".into(),
                params: Vec::new(),
                note: Some(
                    "the `git commit` call is nested inside another command —                      approve the command as-is or deny it"
                        .into(),
                ),
            });
        }
        if segments.len() > 1 {
            return Some(GateMatch {
                kind: "commit".into(),
                params: Vec::new(),
                note: Some(format!(
                    "{} commits in one command — approve as-is or deny",
                    segments.len()
                )),
            });
        }

        let segment = segments[0];
        if let Some(reason) = external_source(segment) {
            return Some(GateMatch {
                kind: "commit".into(),
                params: Vec::new(),
                note: Some(reason),
            });
        }
        let (message_count, _, message_ambiguous) =
            count_flag_detail(segment, "--message", Some("-m"), true);
        if message_ambiguous {
            return Some(GateMatch {
                kind: "commit".into(),
                params: Vec::new(),
                note: Some(
                    "-m is followed by another flag, so git would use that as the message — approve as-is or deny"
                        .into(),
                ),
            });
        }
        if message_count > 1 {
            // git concatenates repeated -m into paragraphs; editing one is misleading.
            return Some(GateMatch {
                kind: "commit".into(),
                params: Vec::new(),
                note: Some("several -m paragraphs in one commit — approve as-is or deny".into()),
            });
        }

        Some(GateMatch {
            kind: "commit".into(),
            params: vec![param(
                PARAM_MESSAGE,
                "Commit message",
                flag_value(segment, "--message", Some("-m"), true),
                true,
            )],
            note: None,
        })
    }

    fn apply(&self, args: &Value, edited: &[GateParam]) -> Value {
        let Some(command) = command_of(args) else {
            return args.clone();
        };
        let tokens = tokenize(command);
        let segments = split_commands(&tokens);
        let Some(segment) = segments
            .into_iter()
            .find(|s| git_subcommand(s).is_some_and(|i| s[i].text == "commit"))
        else {
            return args.clone();
        };
        let mut edits = Vec::new();
        if let Some(value) = edited_value(edited, PARAM_MESSAGE) {
            if let Some(edit) = flag_edit(segment, "--message", Some("-m"), true, value) {
                edits.push(edit);
            }
        }
        with_command(args, splice(command, edits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(command: &str) -> Value {
        json!({ "command": command, "description": "test" })
    }

    fn words(tokens: &[Token]) -> Vec<String> {
        tokens.iter().map(|t| t.text.clone()).collect()
    }

    // ---------- tokenizer ----------

    #[test]
    fn tokenize_plain_words() {
        let tokens = tokenize("git push origin main");
        assert_eq!(words(&tokens), ["git", "push", "origin", "main"]);
        assert!(tokens.iter().all(|t| !t.is_separator));
    }

    #[test]
    fn tokenize_resolves_quotes() {
        let tokens = tokenize(r#"git commit -m "hello world" -t 'single quoted'"#);
        assert_eq!(
            words(&tokens),
            ["git", "commit", "-m", "hello world", "-t", "single quoted"]
        );
    }

    #[test]
    fn tokenize_escaped_quotes_in_double_quotes() {
        let tokens = tokenize(r#"--title "A \"quoted\" title""#);
        assert_eq!(words(&tokens), ["--title", r#"A "quoted" title"#]);
    }

    #[test]
    fn tokenize_marks_separators() {
        let tokens = tokenize("cd x && git push; echo hi | cat");
        let seps: Vec<&str> = tokens
            .iter()
            .filter(|t| t.is_separator)
            .map(|t| t.text.as_str())
            .collect();
        assert_eq!(seps, ["&&", ";", "|"]);
        let segments = split_commands(&tokens);
        assert_eq!(segments.len(), 4);
        assert_eq!(segments[1][0].text, "git");
    }

    #[test]
    fn tokenize_spans_cover_raw_tokens() {
        let command = r#"git commit -m "hello world""#;
        let tokens = tokenize(command);
        let msg = tokens.last().unwrap();
        assert_eq!(&command[msg.start..msg.end], r#""hello world""#);
        let git = &tokens[0];
        assert_eq!(&command[git.start..git.end], "git");
    }

    #[test]
    fn tokenize_keeps_substitution_with_heredoc_as_one_token() {
        let command =
            "git commit -m \"$(cat <<'EOF'\nLine one.\n\nLine (two) with \"quotes\".\nEOF\n)\"";
        let tokens = tokenize(command);
        assert_eq!(tokens.len(), 4);
        let msg = &tokens[3];
        assert!(msg.text.starts_with("$(cat <<'EOF'"));
        assert!(msg.text.ends_with(')'));
        assert_eq!(&command[msg.start..msg.end], &command[14..]);
    }

    #[test]
    fn tokenize_handles_plain_substitution() {
        let tokens = tokenize("echo $(git rev-parse HEAD) done");
        assert_eq!(words(&tokens), ["echo", "$(git rev-parse HEAD)", "done"]);
    }

    // ---------- quoting / heredoc helpers ----------

    #[test]
    fn sh_quote_passes_plain_words_through() {
        assert_eq!(sh_quote("origin/main"), "origin/main");
        assert_eq!(sh_quote("v1.2.3"), "v1.2.3");
    }

    #[test]
    fn sh_quote_wraps_and_escapes() {
        assert_eq!(sh_quote("two words"), "'two words'");
        assert_eq!(sh_quote("it's done"), r"'it'\''s done'");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("a\"b"), "'a\"b'");
    }

    #[test]
    fn unwrap_heredoc_variants() {
        let quoted = "$(cat <<'EOF'\nHello\nworld\nEOF\n)";
        assert_eq!(unwrap_heredoc(quoted).as_deref(), Some("Hello\nworld"));

        let unquoted = "$(cat <<EOF\nsingle line\nEOF\n)";
        assert_eq!(unwrap_heredoc(unquoted).as_deref(), Some("single line"));

        let dashed = "$(cat <<-'MSG'\nbody\nMSG\n)";
        assert_eq!(unwrap_heredoc(dashed).as_deref(), Some("body"));

        let with_quotes = "$(cat <<'EOF'\nHe said \"hi\" and 'bye'.\nEOF\n)";
        assert_eq!(
            unwrap_heredoc(with_quotes).as_deref(),
            Some("He said \"hi\" and 'bye'.")
        );

        assert_eq!(unwrap_heredoc("plain text"), None);
        assert_eq!(unwrap_heredoc("$(git rev-parse HEAD)"), None);
    }

    // ---------- git push rule ----------

    #[test]
    fn git_push_matches_variants() {
        let rule = GitPushRule;
        for command in [
            "git push",
            "git push origin main",
            "git push --force-with-lease",
            "git -C ../elsewhere push",
            "cd repo && git push",
            "git add -A; git commit -m x; git push",
            "git push | tee log.txt",
        ] {
            let m = rule.matches(BASH_TOOL, &bash(command));
            assert!(m.is_some(), "should match: {command}");
            assert_eq!(m.unwrap().kind, "git push");
        }
    }

    #[test]
    fn git_push_ignores_non_push() {
        let rule = GitPushRule;
        for command in [
            "git pull",
            "git status",
            "echo git push",
            "echo \"git push\"",
            "cargo build",
            "git commit -m 'do not push yet'",
        ] {
            assert!(
                rule.matches(BASH_TOOL, &bash(command)).is_none(),
                "should not match: {command}"
            );
        }
    }

    #[test]
    fn git_push_has_no_editable_params_and_identity_apply() {
        let rule = GitPushRule;
        let args = bash("git push origin main");
        let m = rule.matches(BASH_TOOL, &args).unwrap();
        assert!(m.params.is_empty());
        assert_eq!(rule.apply(&args, &[]), args);
    }

    #[test]
    fn rules_ignore_other_tools() {
        let args = json!({ "command": "git push" });
        assert!(GitPushRule.matches("Read", &args).is_none());
        assert!(GhPrCreateRule.matches("Write", &args).is_none());
        assert!(GitCommitRule.matches("Grep", &args).is_none());
    }

    // ---------- gh pr create rule ----------

    fn pr_params(command: &str) -> (String, String) {
        let m = GhPrCreateRule
            .matches(BASH_TOOL, &bash(command))
            .unwrap_or_else(|| panic!("should match: {command}"));
        assert_eq!(m.kind, "PR creation");
        let title = m.params.iter().find(|p| p.key == "title").unwrap();
        let body = m.params.iter().find(|p| p.key == "body").unwrap();
        assert!(!title.multiline);
        assert!(body.multiline);
        (title.value.clone(), body.value.clone())
    }

    #[test]
    fn pr_create_extracts_quoted_title_and_body() {
        let (title, body) =
            pr_params(r#"gh pr create --title "A \"quoted\" title" --body 'multi word'"#);
        assert_eq!(title, r#"A "quoted" title"#);
        assert_eq!(body, "multi word");
    }

    #[test]
    fn pr_create_supports_equals_and_short_flags() {
        let (title, body) = pr_params(r#"gh pr create --title="T one" -b "B one""#);
        assert_eq!(title, "T one");
        assert_eq!(body, "B one");

        let (title, body) = pr_params("gh pr create -t short -b=inline");
        assert_eq!(title, "short");
        assert_eq!(body, "inline");
    }

    #[test]
    fn pr_create_unwraps_heredoc_body() {
        let command =
            "gh pr create --title \"T7 gate\" --body \"$(cat <<'EOF'\n## Summary\n- gate\nEOF\n)\"";
        let (title, body) = pr_params(command);
        assert_eq!(title, "T7 gate");
        assert_eq!(body, "## Summary\n- gate");
    }

    #[test]
    fn pr_create_fill_is_gated_but_not_editable() {
        // --fill takes the title/body from the commits: showing empty fields and
        // appending edited flags would produce a command gh rejects.
        let m = GhPrCreateRule
            .matches(BASH_TOOL, &bash("gh pr create --fill"))
            .expect("should gate");
        assert!(m.params.is_empty(), "no editable params for --fill");
        assert!(m.note.is_some(), "the dialog explains why");
    }

    #[test]
    fn pr_create_bare_command_offers_empty_editable_params() {
        let (title, body) = pr_params("gh pr create");
        assert_eq!(title, "");
        assert_eq!(body, "");
    }

    /// Regression: a backslash-newline continuation used to fuse the next word into
    /// the token (`push\n--force`), so no rule matched and the gate opened silently.
    #[test]
    fn line_continuations_still_match() {
        for command in [
            "git push \\\n  --force-with-lease origin HEAD",
            "git \\\n  push origin main",
            "git \\\r\n  push origin main",
        ] {
            assert!(
                GitPushRule.matches(BASH_TOOL, &bash(command)).is_some(),
                "must gate: {command:?}"
            );
        }
    }

    /// Wrapped, grouped and substituted invocations must all reach the gate; none of
    /// them is editable, so each carries an explanatory note where params would be.
    #[test]
    fn wrapped_invocations_are_gated() {
        for command in [
            "sh -c \"git push origin main\"",
            "bash -c 'git push'",
            "eval \"git push\"",
            "command git push",
            "env git push",
            "time git push",
            "/usr/bin/git push",
            "(git push)",
            "{ git push; }",
            "$(git push)",
            "if true; then git push; fi",
        ] {
            assert!(
                GitPushRule.matches(BASH_TOOL, &bash(command)).is_some(),
                "must gate: {command}"
            );
        }
        // Nested gh pr create is gated but not editable.
        let m = GhPrCreateRule
            .matches(BASH_TOOL, &bash("sh -c \"gh pr create -t A -b B\""))
            .expect("nested pr create must gate");
        assert!(m.params.is_empty());
        assert!(m.note.is_some());
    }

    #[test]
    fn unrelated_git_commands_do_not_match() {
        for command in ["git status", "git log --oneline", "gh pr view 3", "ls -la"] {
            assert!(
                GitPushRule.matches(BASH_TOOL, &bash(command)).is_none(),
                "must not gate: {command}"
            );
            assert!(
                GhPrCreateRule.matches(BASH_TOOL, &bash(command)).is_none(),
                "must not gate: {command}"
            );
        }
    }

    /// Repeated flags or several matching segments would let the user approve one value
    /// while a different one executes; the gate drops editing instead.
    #[test]
    fn ambiguous_commands_are_not_editable() {
        let repeated = GhPrCreateRule
            .matches(
                BASH_TOOL,
                &bash("gh pr create --title A --body B --title EVIL"),
            )
            .expect("must gate");
        assert!(repeated.params.is_empty(), "repeated --title is ambiguous");
        assert!(repeated.note.is_some());

        let two_prs = GhPrCreateRule
            .matches(
                BASH_TOOL,
                &bash("gh pr create -t A -b B; gh pr create -t C -b D"),
            )
            .expect("must gate");
        assert!(two_prs.params.is_empty());

        let two_commits = GitCommitRule
            .matches(
                BASH_TOOL,
                &bash("git commit -m \"a\" && git commit -m \"b\""),
            )
            .expect("must gate");
        assert!(two_commits.params.is_empty(), "two commits are ambiguous");
        assert!(two_commits.note.as_deref().is_some_and(|n| n.contains("2")));
    }

    /// `-F file` / `--body-file` hide the real text: gate, but do not offer an empty
    /// field whose edit would append a mutually exclusive flag.
    #[test]
    fn external_message_sources_are_not_editable() {
        for (rule_kind, command) in [
            ("commit", "git commit -F msg.txt"),
            ("commit", "git commit --file=msg.txt"),
        ] {
            let m = GitCommitRule
                .matches(BASH_TOOL, &bash(command))
                .unwrap_or_else(|| panic!("must gate: {command}"));
            assert_eq!(m.kind, rule_kind);
            assert!(m.params.is_empty(), "not editable: {command}");
            assert!(m.note.is_some(), "note explains why: {command}");
        }
        let pr = GhPrCreateRule
            .matches(BASH_TOOL, &bash("gh pr create -t A --body-file b.md"))
            .expect("must gate");
        assert!(pr.params.is_empty());
    }

    /// A dangling flag used to abort the whole scan, hiding a later real value.
    #[test]
    fn dangling_and_ambiguous_flags_are_handled() {
        // `--title --body B`: gh would take `--body` as the title, so editing is unsafe.
        let ambiguous = GhPrCreateRule
            .matches(BASH_TOOL, &bash("gh pr create --title --body B"))
            .expect("must gate");
        assert!(ambiguous.params.is_empty());
        assert!(ambiguous.note.is_some());

        // A trailing `-m` has no value at all: editable with an empty message.
        let m = GitCommitRule
            .matches(BASH_TOOL, &bash("git commit -m"))
            .expect("must gate");
        assert!(m.params.iter().all(|p| p.value.is_empty()));
    }

    #[test]
    fn pr_create_ignores_other_gh_commands() {
        for command in [
            "gh pr view",
            "gh pr list",
            "gh issue create",
            "gh repo clone x",
        ] {
            assert!(
                GhPrCreateRule.matches(BASH_TOOL, &bash(command)).is_none(),
                "should not match: {command}"
            );
        }
    }

    #[test]
    fn pr_create_apply_round_trips_edited_values() {
        let rule = GhPrCreateRule;
        let args = bash(r#"gh pr create --title "old title" --body "old body" --draft"#);
        let edited = [
            param("title", "Title", "new \"final\" title".into(), false),
            param("body", "Body", "line one\nline two".into(), true),
        ];
        let updated = rule.apply(&args, &edited);
        let command = updated["command"].as_str().unwrap();
        assert!(
            command.contains("--draft"),
            "unrelated flags kept: {command}"
        );

        let (title, body) = pr_params(command);
        assert_eq!(title, "new \"final\" title");
        assert_eq!(body, "line one\nline two");
        assert_eq!(updated["description"], "test");
    }

    #[test]
    fn pr_create_apply_rebuilds_heredoc_body_as_quoted_value() {
        let rule = GhPrCreateRule;
        let args =
            bash("gh pr create -t old --body \"$(cat <<'EOF'\nold body\nEOF\n)\" --base main");
        let edited = [
            param("title", "Title", "new title".into(), false),
            param("body", "Body", "approved body\nwith 'quotes'".into(), true),
        ];
        let updated = rule.apply(&args, &edited);
        let command = updated["command"].as_str().unwrap();
        assert!(command.contains("--base main"));
        assert!(!command.contains("cat <<"), "heredoc replaced: {command}");

        let (title, body) = pr_params(command);
        assert_eq!(title, "new title");
        assert_eq!(body, "approved body\nwith 'quotes'");
    }

    #[test]
    fn pr_create_apply_appends_missing_flags() {
        let rule = GhPrCreateRule;
        let args = bash("gh pr create --draft");
        let edited = [
            param("title", "Title", "added title".into(), false),
            param("body", "Body", "".into(), true),
        ];
        let updated = rule.apply(&args, &edited);
        let command = updated["command"].as_str().unwrap();
        let (title, body) = pr_params(command);
        assert_eq!(title, "added title");
        assert_eq!(body, "", "empty edit is not appended");
    }

    #[test]
    fn pr_create_apply_only_touches_the_pr_segment() {
        let rule = GhPrCreateRule;
        let args = bash(r#"git push -u origin HEAD && gh pr create --title old --body b"#);
        let edited = [
            param("title", "Title", "new".into(), false),
            param("body", "Body", "b".into(), true),
        ];
        let updated = rule.apply(&args, &edited);
        let command = updated["command"].as_str().unwrap();
        assert!(command.starts_with("git push -u origin HEAD && "));
        let (title, _) = pr_params(command);
        assert_eq!(title, "new");
    }

    // ---------- git commit rule ----------

    fn commit_message(command: &str) -> String {
        let m = GitCommitRule
            .matches(BASH_TOOL, &bash(command))
            .unwrap_or_else(|| panic!("should match: {command}"));
        assert_eq!(m.kind, "commit");
        m.params[0].value.clone()
    }

    #[test]
    fn commit_extracts_message_variants() {
        assert_eq!(commit_message(r#"git commit -m "fix: bug""#), "fix: bug");
        assert_eq!(commit_message("git commit --message='the fix'"), "the fix");
        assert_eq!(commit_message("git commit -am 'all in'"), "all in");
        assert_eq!(commit_message(r#"git commit -m"attached""#), "attached");
        assert_eq!(commit_message("git commit"), "");
    }

    #[test]
    fn commit_unwraps_heredoc_message() {
        let command = "git commit -m \"$(cat <<'EOF'\nT7: gate registry\n\nDetails here.\nEOF\n)\"";
        assert_eq!(
            commit_message(command),
            "T7: gate registry\n\nDetails here."
        );
    }

    #[test]
    fn commit_does_not_match_other_git_commands() {
        for command in ["git push", "git status", "npm run commitlint"] {
            assert!(
                GitCommitRule.matches(BASH_TOOL, &bash(command)).is_none(),
                "should not match: {command}"
            );
        }
    }

    #[test]
    fn commit_apply_round_trips_with_quotes_inside_message() {
        let rule = GitCommitRule;
        let args = bash(r#"git add -A && git commit -m "old message" && echo done"#);
        let edited = [param(
            "message",
            "Commit message",
            "T7: it's \"done\"".into(),
            true,
        )];
        let updated = rule.apply(&args, &edited);
        let command = updated["command"].as_str().unwrap();
        assert!(command.starts_with("git add -A && "));
        assert!(command.ends_with(" && echo done"));
        assert_eq!(commit_message(command), "T7: it's \"done\"");
    }

    #[test]
    fn commit_apply_replaces_heredoc_message() {
        let rule = GitCommitRule;
        let args = bash("git commit -m \"$(cat <<'EOF'\nold\nEOF\n)\" --no-verify");
        let edited = [param("message", "Commit message", "approved".into(), true)];
        let updated = rule.apply(&args, &edited);
        let command = updated["command"].as_str().unwrap();
        assert_eq!(command, "git commit -m approved --no-verify");
    }

    #[test]
    fn commit_apply_appends_message_when_absent() {
        let rule = GitCommitRule;
        let args = bash("git commit");
        let edited = [param("message", "Commit message", "typed in".into(), true)];
        let updated = rule.apply(&args, &edited);
        assert_eq!(
            updated["command"].as_str().unwrap(),
            "git commit --message 'typed in'"
        );
    }
}
