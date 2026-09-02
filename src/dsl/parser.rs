//! Parser for the basil DSL.
//!
//! Consumes logical lines (from `crate::dsl::lexer`) and builds a `File` AST.
//! Handles the documented instruction subset and rejects everything explicitly
//! deferred by the design with a clear parse-time error — never silently.

use super::ast::*;
use super::lexer::{lex, LogicalLine};
use std::collections::HashMap;

/// A parse error with the source line it occurred on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for Error {}

pub fn parse(source: &str) -> Result<File, Error> {
    Parser {
        lines: lex(source),
        idx: 0,
    }
    .parse_file()
}

/// Split an instruction line into its keyword and the remainder.
fn split_keyword(line: &str) -> (&str, &str) {
    let s = line.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

struct Parser {
    lines: Vec<LogicalLine>,
    idx: usize,
}

impl Parser {
    fn peek(&self) -> Option<&LogicalLine> {
        self.lines.get(self.idx)
    }

    fn bump(&mut self) -> Option<LogicalLine> {
        let l = self.lines.get(self.idx).cloned();
        self.idx += 1;
        l
    }

    fn err<T>(&self, line: usize, message: impl Into<String>) -> Result<T, Error> {
        Err(Error {
            line,
            message: message.into(),
        })
    }

    fn err_at(&self, line: usize, message: impl Into<String>) -> Error {
        Error { line, message: message.into() }
    }

    fn parse_file(mut self) -> Result<File, Error> {
        let mut targets = Vec::new();
        let mut reactions = Vec::new();
        let mut default_platforms = Vec::new();
        let mut current: Option<Target> = None;

        while let Some(line) = self.peek().cloned() {
            let (kw, rest) = split_keyword(&line.text);

            let top_level = matches!(kw, "FROM" | "ON");
            if top_level {
                if let Some(t) = current.take() {
                    targets.push(t);
                }
            }

            match kw {
                "FROM" => {
                    self.bump();
                    current = Some(self.parse_from(&line.text, line.number)?);
                }
                "ON" => {
                    self.bump();
                    reactions.push(self.parse_on(rest, line.number)?);
                }
                "PLATFORM" => {
                    self.bump();
                    let platforms = self.parse_platforms(rest, line.number)?;
                    match &mut current {
                        Some(target) => {
                            if target.platforms.is_some() {
                                return self.err(line.number, "duplicate PLATFORM in target");
                            }
                            target.platforms = Some(platforms);
                        }
                        None => default_platforms.extend(platforms),
                    }
                }
                "TASK" if current.is_none() => {
                    return self.err(line.number, "TASK must appear inside a target (after FROM)");
                }
                _ if !is_known_or_deferred(kw) => {
                    return self.err(line.number, format!("unexpected directive `{kw}`"));
                }
                _ => {
                    let target = current.as_mut().ok_or_else(|| Error {
                        line: line.number,
                        message: format!(
                            "`{kw}` outside any target — the first instruction must be FROM"
                        ),
                    })?;
                    if kw == "TASK" {
                        if target.task.is_some() {
                            return self.err(line.number, "duplicate TASK in target");
                        }
                        target.task = Some(TaskCmd {
                            command: self.parse_command(rest, line.number)?,
                        });
                    } else {
                        self.parse_instruction(target, kw, rest, line.number)?;
                    }
                    self.bump();
                }
            }
        }

        if let Some(t) = current.take() {
            targets.push(t);
        }

        Ok(File {
            targets,
            reactions,
            default_platforms,
        })
    }

    fn parse_from(&self, full_line: &str, line: usize) -> Result<Target, Error> {
        let (_, rest) = split_keyword(full_line);
        let mut parts = split_ws(rest);
        if parts.is_empty() {
            return self.err(line, "FROM requires a base image");
        }
        let base_tok = parts.remove(0);
        let mut name: Option<String> = None;
        if !parts.is_empty() {
            if parts[0].eq_ignore_ascii_case("AS") {
                if parts.len() < 2 {
                    return self.err(line, "FROM ... AS requires a stage name");
                }
                name = Some(parts[1].clone());
                if parts.len() > 2 {
                    return self.err(line, "unexpected token after stage name");
                }
            } else {
                return self.err(line, format!("unexpected token after base `{base_tok}`"));
            }
        }

        let (raw, digest) = split_digest(&base_tok);
        let (raw, tag) = split_tag(&raw);

        Ok(Target {
            name: name.unwrap_or_else(|| raw.clone()),
            base: BaseImage { raw, tag, digest },
            instructions: Vec::new(),
            task: None,
            platforms: None,
            line,
        })
    }

    fn parse_instruction(
        &self,
        target: &mut Target,
        kw: &str,
        rest: &str,
        line: usize,
    ) -> Result<(), Error> {
        match kw {
            "SHELL" | "STOPSIGNAL" | "HEALTHCHECK" | "ONBUILD" => {
                return self.err(
                    line,
                    format!("`{kw}` is not supported in basil v1 (out of emitter scope)"),
                )
            }
            _ => {}
        }

        let instr = match kw {
            "ARG" => self.parse_arg(rest, line)?,
            "ENV" => self.parse_env(rest, line)?,
            "COPY" => self.parse_copy(rest, line)?,
            "ADD" => self.parse_add(rest, line)?,
            "RUN" => self.parse_run(rest, line)?,
            "CMD" => Instruction::Cmd(self.parse_command(rest, line)?),
            "ENTRYPOINT" => Instruction::Entrypoint(self.parse_command(rest, line)?),
            "USER" => self.parse_user(rest, line)?,
            "WORKDIR" => self.parse_workdir(rest, line)?,
            "LABEL" => self.parse_label(rest, line)?,
            "EXPOSE" => {
                let words = self.parse_wordlist(rest, line)?;
                Instruction::Expose(words)
            }
            "VOLUME" => self.parse_volume(rest, line)?,
            _ => {
                return self.err(line, format!("unknown instruction `{kw}`"))
            }
        };
        target.instructions.push(instr);
        Ok(())
    }

    fn parse_arg(&self, rest: &str, line: usize) -> Result<Instruction, Error> {
        let s = rest.trim();
        if s.is_empty() {
            return self.err(line, "ARG requires a name");
        }
        if s.split_whitespace().count() > 1 {
            return self.err(line, "ARG supports one name per line (name or name=default)");
        }
        let (name, default) = match s.split_once('=') {
            Some((n, d)) => (n.trim().to_string(), Some(d.trim().to_string())),
            None => (s.to_string(), None),
        };
        if name.is_empty() {
            return self.err(line, "ARG name is empty");
        }
        Ok(Instruction::Arg(Arg { name, default, line }))
    }

    fn parse_env(&self, rest: &str, line: usize) -> Result<Instruction, Error> {
        let s = rest.trim();
        if s.is_empty() {
            return self.err(line, "ENV requires at least one key");
        }
        let tokens = split_ws_tokens(s).map_err(|m| self.err_at(line, m))?;
        let mut pairs = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            let tok = &tokens[i];
            if let Some((k, v)) = tok.split_once('=') {
                if k.is_empty() {
                    return self.err(line, "ENV key is empty");
                }
                pairs.push((k.to_string(), v.to_string()));
                i += 1;
            } else {
                if i + 1 >= tokens.len() {
                    return self.err(line, format!("ENV `{tok}` has no value"));
                }
                pairs.push((tok.clone(), tokens[i + 1].clone()));
                i += 2;
            }
        }
        Ok(Instruction::Env(Env { pairs }))
    }

    fn parse_copy(&self, rest: &str, line: usize) -> Result<Instruction, Error> {
        if detect_heredoc(rest) {
            return self.err(line, "COPY heredocs (<<EOF) are deferred in basil v1");
        }
        let (flags, args) = parse_flags(rest).map_err(|m| self.err_at(line, m))?;
        for name in flags.keys() {
            match name.as_str() {
                "from" | "chown" | "chmod" => {}
                "link" => {
                    return self.err(line, "COPY --link is deferred in basil v1");
                }
                _ => {
                    return self.err(line, format!("unsupported COPY flag --{name}"));
                }
            }
        }
        if args.len() < 2 {
            return self.err(line, "COPY requires at least one source and a destination");
        }
        let dest = args.last().unwrap().clone();
        let sources = args[..args.len() - 1].to_vec();
        Ok(Instruction::Copy(Copy {
            from: flags.get("from").cloned(),
            chown: flags.get("chown").cloned(),
            chmod: flags.get("chmod").cloned(),
            sources,
            dest,
        }))
    }

    fn parse_add(&self, rest: &str, line: usize) -> Result<Instruction, Error> {
        if detect_heredoc(rest) {
            return self.err(line, "ADD heredocs (<<EOF) are deferred in basil v1");
        }
        let (flags, args) = parse_flags(rest).map_err(|m| self.err_at(line, m))?;
        for name in flags.keys() {
            match name.as_str() {
                "chown" => {}
                _ => return self.err(line, format!("unsupported ADD flag --{name}")),
            }
        }
        if args.len() < 2 {
            return self.err(line, "ADD requires at least one source and a destination");
        }
        let dest = args.last().unwrap().clone();
        let sources = args[..args.len() - 1].to_vec();
        Ok(Instruction::Add(Add {
            chown: flags.get("chown").cloned(),
            sources,
            dest,
        }))
    }

    fn parse_run(&self, rest: &str, line: usize) -> Result<Instruction, Error> {
        if detect_heredoc(rest) {
            return self.err(line, "RUN heredocs (<<EOF) are deferred in basil v1");
        }
        let mut mounts = Vec::new();
        let mut command_text = rest.trim().to_string();
        loop {
            let t = command_text.trim_start();
            if t.starts_with("--mount=") {
                let (spec, rest2) = take_mount_spec(t);
                mounts.push(self.parse_mount(&spec, line)?);
                command_text = rest2;
            } else if t.starts_with("--network=") {
                return self.err(line, "RUN --network is deferred in basil v1");
            } else if t.starts_with("--security=") {
                return self.err(line, "RUN --security is deferred in basil v1");
            } else if t.starts_with("--") {
                return self.err(line, "unsupported RUN flag");
            } else {
                break;
            }
        }
        if command_text.trim().is_empty() {
            return self.err(line, "RUN requires a command");
        }
        let command = self.parse_command(&command_text, line)?;
        Ok(Instruction::Run(Run { mounts, command }))
    }

    fn parse_mount(&self, spec: &str, line: usize) -> Result<Mount, Error> {
        let entries = parse_mount_map(spec);
        let typev = entries.get("type").cloned().unwrap_or_else(|| "bind".to_string());
        let target = entries.get("target").cloned();
        let id = entries.get("id").cloned();
        match typev.as_str() {
            "bind" => {
                let t = target.ok_or_else(|| self.err_at(line, "bind mount requires target"))?;
                Ok(Mount::Bind {
                    source: entries.get("source").cloned(),
                    target: t,
                    read_only: entries
                        .get("readonly")
                        .map(|v| v == "true" || v == "1")
                        .unwrap_or(false),
                })
            }
            "secret" => Ok(Mount::Secret {
                id: id.ok_or_else(|| self.err_at(line, "secret mount requires id"))?,
                target,
            }),
            "ssh" => Ok(Mount::Ssh { id }),
            "cache" => {
                let t = target.ok_or_else(|| self.err_at(line, "cache mount requires target"))?;
                Ok(Mount::Cache { id, target: t })
            }
            other => Err(self.err_at(line, format!("unsupported mount type `{other}`"))),
        }
    }

    fn parse_command(&self, rest: &str, line: usize) -> Result<Command, Error> {
        if detect_heredoc(rest) {
            return self.err(line, "heredocs are deferred in basil v1");
        }
        parse_command_after(rest.trim()).map_err(|m| self.err_at(line, m))
    }

    fn parse_user(&self, rest: &str, line: usize) -> Result<Instruction, Error> {
        let s = rest.trim();
        if s.is_empty() {
            return self.err(line, "USER requires a value");
        }
        Ok(Instruction::User(s.to_string()))
    }

    fn parse_workdir(&self, rest: &str, line: usize) -> Result<Instruction, Error> {
        let s = rest.trim();
        if s.is_empty() {
            return self.err(line, "WORKDIR requires a path");
        }
        Ok(Instruction::Workdir(Workdir { path: s.to_string() }))
    }

    fn parse_label(&self, rest: &str, line: usize) -> Result<Instruction, Error> {
        let items = split_ws_tokens(rest).map_err(|m| self.err_at(line, m))?;
        if items.is_empty() {
            return self.err(line, "LABEL requires at least one key=value");
        }
        let mut pairs = Vec::new();
        for tok in items {
            match tok.split_once('=') {
                Some((k, v)) => pairs.push((k.to_string(), v.to_string())),
                None => {
                    return self.err(line, format!("LABEL expects key=value, got `{tok}`"))
                }
            }
        }
        Ok(Instruction::Label(pairs))
    }

    fn parse_wordlist(&self, rest: &str, line: usize) -> Result<Vec<String>, Error> {
        let words = split_ws_tokens(rest).map_err(|m| self.err_at(line, m))?;
        if words.is_empty() {
            return self.err(line, "expected at least one value");
        }
        Ok(words)
    }

    fn parse_volume(&self, rest: &str, line: usize) -> Result<Instruction, Error> {
        Ok(Instruction::Volume(self.parse_wordlist(rest, line)?))
    }

    fn parse_on(&self, rest: &str, line: usize) -> Result<Reaction, Error> {
        let (event, tail) = parse_event_match(rest).map_err(|m| self.err_at(line, m))?;
        let (build, do_list) = parse_verb_lists(&tail).map_err(|m| self.err_at(line, m))?;
        if build.is_empty() && do_list.is_empty() {
            return self.err(line, "reaction has neither BUILD nor DO");
        }
        Ok(Reaction {
            event,
            build,
            do_list,
            line,
        })
    }

    fn parse_platforms(&self, rest: &str, line: usize) -> Result<Vec<Platform>, Error> {
        let words = split_ws_tokens(rest).map_err(|m| self.err_at(line, m))?;
        if words.is_empty() {
            return self.err(line, "PLATFORM requires at least one platform");
        }
        words
            .into_iter()
            .map(|w| parse_platform_token(&w).map_err(|m| self.err_at(line, m)))
            .collect()
    }
}

fn is_known_or_deferred(kw: &str) -> bool {
    matches!(
        kw,
        "ARG"
            | "ENV"
            | "COPY"
            | "ADD"
            | "RUN"
            | "CMD"
            | "ENTRYPOINT"
            | "USER"
            | "WORKDIR"
            | "LABEL"
            | "EXPOSE"
            | "VOLUME"
            | "TASK"
            | "SHELL"
            | "STOPSIGNAL"
            | "HEALTHCHECK"
            | "ONBUILD"
    )
}

/// Parse `event(refiner) BUILD ...` into (EventMatch, remaining-words string).
/// The token is the leading identifier; a selector list is balanced parens that
/// may contain spaces (e.g. `push(main, develop)`).
fn parse_event_match(rest: &str) -> Result<(EventMatch, String), String> {
    let s = rest.trim();
    let chars: Vec<char> = s.chars().collect();

    // Token: characters up to the first whitespace or '('.
    let mut token_len = 0;
    while token_len < chars.len() {
        let c = chars[token_len];
        if c.is_whitespace() || c == '(' {
            break;
        }
        token_len += 1;
    }
    let token: String = chars[..token_len].iter().collect();

    let mut selectors = Vec::new();
    let after;
    let i = token_len;
    if i < chars.len() && chars[i] == '(' {
        // Consume the balanced group.
        let mut depth = 0usize;
        let sel_start = i + 1;
        let mut sel_end = None;
        let mut j = i;
        while j < chars.len() {
            match chars[j] {
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        sel_end = Some(j);
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let end = sel_end.ok_or("unterminated selector list")?;
        let inner: String = chars[sel_start..end].iter().collect();
        selectors = split_selectors(&inner);
        let after_chars = &chars[end + 1..];
        after = after_chars.iter().collect::<String>().trim_start().to_string();
    } else {
        after = chars[i..].iter().collect::<String>().trim_start().to_string();
    }

    let event = EventMatch {
        token: classify_token(&token),
        selectors,
    };
    Ok((event, after))
}

fn classify_token(token: &str) -> EventToken {
    match token {
        "pr-opened" => EventToken::PrOpened,
        "pr-synchronized" => EventToken::PrSynchronized,
        "pr-merged" => EventToken::PrMerged,
        "push" => EventToken::Push,
        "tag" => EventToken::Tag,
        t => EventToken::Custom(t.to_string()),
    }
}

fn split_selectors(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// Parse the `BUILD a b DO c d` tail into (build, do_list).
fn parse_verb_lists(tail: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let tokens = split_ws_tokens(tail)?;
    let mut build = Vec::new();
    let mut do_list = Vec::new();
    let mut verb: Option<&str> = None;
    for w in tokens {
        let w = w.trim_end_matches(',').to_string();
        if w.eq_ignore_ascii_case("BUILD") {
            verb = Some("build");
        } else if w.eq_ignore_ascii_case("DO") {
            verb = Some("do");
        } else {
            match verb {
                Some("build") => build.push(w),
                Some("do") => do_list.push(w),
                _ => return Err(format!("unexpected token `{w}` before BUILD/DO")),
            }
        }
    }
    Ok((build, do_list))
}

// ---- tokenising / parsing helpers ----

fn split_ws(s: &str) -> Vec<String> {
    s.split_whitespace().map(|t| t.to_string()).collect()
}

/// Split into whitespace-delimited tokens, honoring double quotes for values
/// that may contain spaces (e.g. LABEL, ENV, COPY with quoted paths).
fn split_ws_tokens(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut tok = String::new();
        if c == '"' {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some('"') => tok.push('"'),
                        Some('\\') => tok.push('\\'),
                        Some('n') => tok.push('\n'),
                        Some('t') => tok.push('\t'),
                        Some(other) => {
                            tok.push('\\');
                            tok.push(other);
                        }
                        None => return Err("unterminated quote".to_string()),
                    },
                    Some(c2) => tok.push(c2),
                    None => return Err("unterminated quote".to_string()),
                }
            }
        } else {
            while let Some(&c2) = chars.peek() {
                if c2.is_whitespace() {
                    break;
                }
                tok.push(c2);
                chars.next();
            }
        }
        out.push(tok);
    }
    Ok(out)
}

fn split_digest(s: &str) -> (String, Option<String>) {
    match s.split_once('@') {
        Some((b, d)) => (b.to_string(), Some(d.to_string())),
        None => (s.to_string(), None),
    }
}

fn split_tag(s: &str) -> (String, Option<String>) {
    // A tag is text after the last ':' that is not a host:port and has no '/'
    // in it. Handle `alpine:3.23`, `ghcr.io/a/b:v1`, `localhost:5000/img`.
    match s.rfind(':') {
        Some(idx) => {
            let before = &s[..idx];
            let after = &s[idx + 1..];
            if after.is_empty() || after.contains('/') {
                // port in `localhost:5000/img`, or nothing after ':'
                (s.to_string(), None)
            } else if before.contains(':') {
                // `localhost:5000/img` — the final ':' binds a port, after has
                // no '/', treat as no-tag image.
                (s.to_string(), None)
            } else if before.contains('/') {
                // `ghcr.io/a/b:v1` → tag
                (before.to_string(), Some(after.to_string()))
            } else {
                // `alpine:3.23` → tag
                (before.to_string(), Some(after.to_string()))
            }
        }
        None => (s.to_string(), None),
    }
}

fn parse_platform_token(s: &str) -> Result<Platform, String> {
    let parts: Vec<&str> = s.split('/').collect();
    match parts.as_slice() {
        [os, arch] => Ok(Platform {
            os: os.to_string(),
            arch: arch.to_string(),
            variant: None,
        }),
        [os, arch, variant] => Ok(Platform {
            os: os.to_string(),
            arch: arch.to_string(),
            variant: Some(variant.to_string()),
        }),
        _ => Err(format!("invalid platform `{s}`")),
    }
}

/// Parse leading `--name=value` flags and positional args.
fn parse_flags(s: &str) -> Result<(HashMap<String, String>, Vec<String>), String> {
    let tokens = split_ws_tokens(s)?;
    let mut flags = HashMap::new();
    let mut args = Vec::new();
    for t in tokens {
        if let Some(rest) = t.strip_prefix("--") {
            if let Some((k, v)) = rest.split_once('=') {
                if k.is_empty() {
                    return Err(format!("empty flag name in `{t}`"));
                }
                flags.insert(k.to_string(), v.to_string());
            } else {
                flags.insert(rest.to_string(), String::new());
            }
        } else {
            args.push(t);
        }
    }
    Ok((flags, args))
}

fn detect_heredoc(s: &str) -> bool {
    s.contains("<<")
}

fn parse_mount_map(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for part in s.split(',') {
        if let Some((k, v)) = part.split_once('=') {
            out.insert(k.trim().to_string(), v.trim().to_string());
        } else if !part.trim().is_empty() {
            // boolean flag like `readonly`
            out.insert(part.trim().to_string(), "true".to_string());
        }
    }
    out
}

/// Take the whole `--mount=<spec>` value (up to the next whitespace) and the
/// remainder. A mount spec contains commas, so it cannot be split on `,`.
fn take_mount_spec(s: &str) -> (String, String) {
    let rest = &s["--mount=".len()..];
    match rest.find(char::is_whitespace) {
        Some(i) => (rest[..i].to_string(), rest[i..].trim_start().to_string()),
        None => (rest.to_string(), String::new()),
    }
}

/// Parse a command: JSON exec form (`["a","b"]`) or shell form.
fn parse_command_after(s: &str) -> Result<Command, String> {
    let s = s.trim();
    if s.starts_with('[') {
        if !s.trim_end().ends_with(']') {
            return Err("unterminated exec command array".to_string());
        }
        let inner = &s[1..s.rfind(']').unwrap()];
        let tokens = split_json_strings(inner)?;
        if tokens.is_empty() {
            return Err("empty exec command".to_string());
        }
        Ok(Command::Exec(tokens))
    } else {
        Ok(Command::Shell(s.to_string()))
    }
}

/// Split a JSON array of strings (inner content, no brackets) into the strings.
fn split_json_strings(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut it = s.trim().chars().peekable();
    loop {
        while matches!(it.peek(), Some(&c) if c.is_whitespace() || c == ',') {
            it.next();
        }
        match it.next() {
            None => break,
            Some('"') => {
                let mut val = String::new();
                loop {
                    match it.next() {
                        Some('"') => break,
                        Some('\\') => match it.next() {
                            Some('"') => val.push('"'),
                            Some('\\') => val.push('\\'),
                            Some('n') => val.push('\n'),
                            Some('t') => val.push('\t'),
                            Some(other) => {
                                val.push('\\');
                                val.push(other);
                            }
                            None => return Err("unterminated string in exec".to_string()),
                        },
                        Some(c2) => val.push(c2),
                        None => return Err("unterminated string in exec".to_string()),
                    }
                }
                out.push(val);
            }
            Some(_) => return Err("exec command must be an array of strings".to_string()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_trivial_from() {
        let f = parse("FROM alpine\n").unwrap();
        assert_eq!(f.targets.len(), 1);
        assert_eq!(f.targets[0].name, "alpine");
        assert_eq!(f.targets[0].base.raw, "alpine");
    }

    #[test]
    fn parses_from_as() {
        let f = parse("FROM golang AS build\n").unwrap();
        assert_eq!(f.targets[0].name, "build");
        assert_eq!(f.targets[0].base.raw, "golang");
    }

    #[test]
    fn parses_target_with_instructions() {
        let src = "FROM golang AS build\nCOPY . /src\nRUN go build -o /src/app\n";
        let f = parse(src).unwrap();
        let t = &f.targets[0];
        assert_eq!(t.name, "build");
        assert_eq!(t.instructions.len(), 2);
        assert!(matches!(t.instructions[0], Instruction::Copy(_)));
        assert!(matches!(t.instructions[1], Instruction::Run(_)));
    }

    #[test]
    fn parses_external_ref_with_tag_and_digest() {
        let f = parse("FROM alpine:3.23@sha256:abc123\n").unwrap();
        let b = &f.targets[0].base;
        assert_eq!(b.raw, "alpine");
        assert_eq!(b.tag.as_deref(), Some("3.23"));
        assert_eq!(b.digest.as_deref(), Some("sha256:abc123"));
    }

    #[test]
    fn parses_qualified_image_bare() {
        // `FROM ghcr.io/a/b:v1` → tag v1
        let f = parse("FROM ghcr.io/a/b:v1 AS app\n").unwrap();
        let b = &f.targets[0].base;
        assert_eq!(b.raw, "ghcr.io/a/b");
        assert_eq!(b.tag.as_deref(), Some("v1"));
    }

    #[test]
    fn parses_localhost_port_image() {
        // `localhost:5000/img` — the ':' is a port, no tag.
        let f = parse("FROM localhost:5000/img AS app\n").unwrap();
        let b = &f.targets[0].base;
        assert_eq!(b.raw, "localhost:5000/img");
        assert_eq!(b.tag, None);
    }

    #[test]
    fn parses_qualified_image_as_name() {
        let f = parse("FROM scratch AS ghcr.io/mycompany/app\n").unwrap();
        assert_eq!(f.targets[0].name, "ghcr.io/mycompany/app");
        assert_eq!(f.targets[0].base.raw, "scratch");
    }

    #[test]
    fn parses_task_clause() {
        let src = "FROM alpine AS notify\nRUN echo hi\nTASK notify-my-thing.sh\n";
        let f = parse(src).unwrap();
        let t = &f.targets[0];
        assert!(matches!(&t.task, Some(TaskCmd { command: Command::Shell(s) }) if s == "notify-my-thing.sh"));
    }

    #[test]
    fn parses_on_reaction() {
        let src = "ON pr-merged BUILD ghcr.io/mycompany/app DO migrate\n";
        let f = parse(src).unwrap();
        assert_eq!(f.reactions.len(), 1);
        let r = &f.reactions[0];
        assert!(matches!(r.event.token, EventToken::PrMerged));
        assert_eq!(r.build, vec!["ghcr.io/mycompany/app"]);
        assert_eq!(r.do_list, vec!["migrate"]);
    }

    #[test]
    fn parses_on_with_selector() {
        let src = "ON push(main, develop) BUILD app\n";
        let f = parse(src).unwrap();
        let r = &f.reactions[0];
        assert!(matches!(r.event.token, EventToken::Push));
        assert_eq!(r.event.selectors, vec!["main", "develop"]);
        assert_eq!(r.build, vec!["app"]);
    }

    #[test]
    fn parses_platforms_default() {
        let src = "PLATFORM linux/amd64\nFROM alpine\n";
        let f = parse(src).unwrap();
        assert_eq!(f.default_platforms.len(), 1);
        assert_eq!(f.targets[0].platforms, None);
    }

    #[test]
    fn parses_target_platforms() {
        let src = "PLATFORM linux/amd64\nFROM alpine\nPLATFORM linux/arm64/v8\n";
        let f = parse(src).unwrap();
        assert_eq!(f.targets[0].platforms.as_ref().unwrap()[0].variant, Some("v8".to_string()));
    }

    #[test]
    fn rejects_instruction_outside_target() {
        let e = parse("RUN echo hi\n").unwrap_err();
        assert!(e.message.contains("outside any target"), "{}", e.message);
    }

    #[test]
    fn rejects_deferred_shell() {
        let src = "FROM alpine\nSHELL [\"/bin/bash\"]\n";
        let e = parse(src).unwrap_err();
        assert!(e.message.contains("not supported"), "{}", e.message);
    }

    #[test]
    fn rejects_copy_heredoc() {
        let src = "FROM alpine\nCOPY <<EOF /x\n";
        let e = parse(src).unwrap_err();
        assert!(e.message.contains("heredoc"), "{}", e.message);
    }

    #[test]
    fn rejects_copy_link() {
        let src = "FROM alpine\nCOPY --link . /x\n";
        let e = parse(src).unwrap_err();
        assert!(e.message.contains("COPY --link"), "{}", e.message);
    }

    #[test]
    fn rejects_run_network() {
        let src = "FROM alpine\nRUN --network=host echo hi\n";
        let e = parse(src).unwrap_err();
        assert!(e.message.contains("--network"), "{}", e.message);
    }

    #[test]
    fn parses_run_with_mount() {
        let src = "FROM alpine\nRUN --mount=type=bind,source=.,target=/src go build\n";
        let f = parse(src).unwrap();
        let run = &f.targets[0].instructions[0];
        if let Instruction::Run(r) = run {
            assert!(matches!(r.mounts[0], Mount::Bind { .. }));
            assert!(matches!(r.command, Command::Shell(_)));
        } else {
            panic!("expected Run");
        }
    }

    #[test]
    fn command_exec_and_shell() {
        let f = parse("FROM alpine\nCMD [\"/bin/sh\", \"-c\", \"echo\"]\nENTRYPOINT echo\n").unwrap();
        let c1 = &f.targets[0].instructions[0];
        let c2 = &f.targets[0].instructions[1];
        assert!(matches!(c1, Instruction::Cmd(Command::Exec(_))));
        assert!(matches!(c2, Instruction::Entrypoint(Command::Shell(_))));
    }

    #[test]
    fn parses_multiple_targets() {
        let src = "FROM golang AS build\nCOPY . /src\n\nFROM scratch AS ghcr.io/mycompany/app\nCOPY --from=build /src/app /app\n";
        let f = parse(src).unwrap();
        assert_eq!(f.targets.len(), 2);
        let t2 = &f.targets[1];
        assert_eq!(t2.name, "ghcr.io/mycompany/app");
        if let Instruction::Copy(c) = &t2.instructions[0] {
            assert_eq!(c.from.as_deref(), Some("build"));
        } else {
            panic!("expected Copy");
        }
    }

    #[test]
    fn parses_full_design_example() {
        let src = "\
FROM golang AS build
COPY . /src
RUN go build -o /src/app

FROM scratch AS ghcr.io/mycompany/app
COPY --from=build /src/app /app

FROM ghcr.io/mycompany/app AS smoke
TASK [ \"/app\", \"--smoke\" ]

FROM alpine AS notify
COPY notify-my-thing.sh /usr/local/bin/
TASK notify-my-thing.sh

FROM alpine AS migrate
COPY migrate.sh /usr/local/bin/
TASK migrate.sh

ON pr-merged        BUILD ghcr.io/mycompany/app   DO migrate
ON image-published  DO notify smoke
";
        let f = parse(src).unwrap();
        assert_eq!(f.targets.len(), 5);
        assert_eq!(f.reactions.len(), 2);
        // notify task target has internal name.
        // image-published is a Custom token.
        assert!(matches!(f.reactions[1].event.token, EventToken::Custom(_)));
        assert_eq!(f.reactions[1].do_list.len(), 2);
        assert!(matches!(f.targets[2].task.as_ref().unwrap().command, Command::Exec(_)));
    }
}