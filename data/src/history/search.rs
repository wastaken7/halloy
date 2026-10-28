use std::fmt::Write;
use std::ops::Range as Span;

use chrono::{DateTime, Local, NaiveDate, TimeDelta, TimeZone, Utc};
use fancy_regex::{Regex, RegexBuilder};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::{Server, message};

const MIN_TEXT_LENGTH: usize = 2;

pub const PAGE_SIZE: usize = 100;

const FILTERS: &[&str] = &[
    "from", "in", "server", "is", "after", "before", "on", "case",
];

#[derive(Debug, Clone)]
pub struct Page {
    pub hits: Vec<Hit>,
    pub newest: Option<Cursor>,
    pub next: Option<Cursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub(super) key: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Latest,
    Before(Cursor),
    After(Cursor),
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub server: String,
    pub buffer: Buffer,
    pub message: message::Message,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Buffer {
    Server,
    Channel(String),
    Query(String),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Database(String),
    #[error("search stopped")]
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferKind {
    Channel,
    Query,
}

impl BufferKind {
    pub(super) fn tag(self) -> &'static str {
        match self {
            BufferKind::Channel => "channel",
            BufferKind::Query => "query",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Query {
    /// AND of groups, each group an OR of terms.
    groups: Vec<Vec<Term>>,
    excluded: Vec<Term>,
    pub(super) from: Vec<String>,
    pub(super) not_from: Vec<String>,
    pub(super) within: Vec<String>,
    servers: Vec<String>,
    pub(super) kind: Option<BufferKind>,
    pub(super) after: Option<DateTime<Utc>>,
    pub(super) before: Option<DateTime<Utc>>,
    regex: Option<String>,
    case_sensitive: bool,
    matcher: Option<Matcher>,
    errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Term {
    text: String,
    /// The last word as it's typed matches words it starts.
    prefix: bool,
}

impl Query {
    pub fn parse(input: &str) -> Self {
        Self::parse_in(input, &Local)
    }

    pub fn parse_in<Tz: TimeZone>(input: &str, tz: &Tz) -> Self {
        let mut query = Query::default();
        let mut pending_or = false;
        let mut last_was_term = false;
        let tokens = lex(input);

        for token in &tokens {
            let mut is_term = false;

            match &token.kind {
                TokenKind::Or => {
                    pending_or = last_was_term;
                    last_was_term = false;
                    continue;
                }
                TokenKind::Word(text) | TokenKind::Phrase(text)
                    if !has_text(text) => {}
                TokenKind::Word(text) | TokenKind::Phrase(text) => {
                    let term = Term {
                        text: text.clone(),
                        prefix: false,
                    };
                    if token.negated {
                        query.excluded.push(term);
                    } else if pending_or
                        && let Some(group) = query.groups.last_mut()
                    {
                        group.push(term);
                        is_term = true;
                    } else {
                        query.groups.push(vec![term]);
                        is_term = true;
                    }
                }
                TokenKind::Regex(pattern) => {
                    if token.negated {
                        query.errors.push("-/regex/ is not supported".into());
                    } else if query.regex.is_some() {
                        query.errors.push("only one /regex/ is allowed".into());
                    } else {
                        query.regex = Some(pattern.clone());
                    }
                }
                TokenKind::Filter(key, value) => {
                    query.filter(key, value, token.negated, tz);
                }
            }

            pending_or = false;
            last_was_term = is_term;
        }

        // A positive word always ends the last group.
        let prefix = tokens.last().is_some_and(|token| {
            !token.negated
                && token.span.end == input.len()
                && matches!(&token.kind, TokenKind::Word(text) if has_text(text))
        });
        if prefix
            && let Some(term) =
                query.groups.last_mut().and_then(|group| group.last_mut())
        {
            term.prefix = true;
        }

        query.matcher = query.build_matcher();

        query
    }

    fn filter<Tz: TimeZone>(
        &mut self,
        key: &str,
        value: &str,
        negated: bool,
        tz: &Tz,
    ) {
        if value.is_empty() {
            return;
        }

        if negated && key != "from" {
            self.errors.push(format!("-{key}: is not supported"));
            return;
        }

        match key {
            "from" if negated => self.not_from.push(value.to_lowercase()),
            "from" => self.from.push(value.to_lowercase()),
            "in" => self.within.push(value.to_owned()),
            "server" => self.servers.push(value.to_owned()),
            "is" => match value.to_lowercase().as_str() {
                "channel" => self.kind = Some(BufferKind::Channel),
                "query" => self.kind = Some(BufferKind::Query),
                _ => self.errors.push(format!(
                    "is:{value} is not valid (use is:channel or is:query)"
                )),
            },
            "case" => match value.to_lowercase().as_str() {
                "sensitive" => self.case_sensitive = true,
                "insensitive" => self.case_sensitive = false,
                _ => self.errors.push(format!(
                    "case:{value} is not valid (use case:sensitive)"
                )),
            },
            "after" | "before" | "on" => {
                let Some((start, end)) = day_bounds(value, tz) else {
                    self.errors.push(format!(
                        "{key}:{value} is not a valid date (use YYYY-MM-DD)"
                    ));
                    return;
                };
                let (after, before) = match key {
                    "after" => (Some(start), None),
                    "before" => (None, Some(start)),
                    _ => (Some(start), Some(end)),
                };
                if let Some(after) = after {
                    self.after = self.after.max(Some(after));
                }
                if let Some(before) = before {
                    self.before = Some(
                        self.before
                            .map_or(before, |current| current.min(before)),
                    );
                }
            }
            _ => {}
        }
    }

    /// Checks what the full-text index can't, or doesn't when nothing
    /// positive narrows it down.
    fn build_matcher(&mut self) -> Option<Matcher> {
        let regex = match &self.regex {
            Some(pattern) => match RegexBuilder::new(pattern)
                .case_insensitive(!self.case_sensitive)
                .build()
            {
                Ok(regex) => Some(regex),
                Err(error) => {
                    self.errors.push(format!("invalid regex: {error}"));
                    return None;
                }
            },
            None => None,
        };

        let unindexed = !self.has_positive();
        let patterns = |terms: &[Term]| {
            terms
                .iter()
                .map(|term| Pattern::new(term, self.case_sensitive))
                .collect::<Vec<_>>()
        };

        let groups = if self.case_sensitive {
            self.groups.iter().map(|group| patterns(group)).collect()
        } else {
            vec![]
        };
        let excluded = if self.case_sensitive || unindexed {
            patterns(&self.excluded)
        } else {
            vec![]
        };
        let not_from = if unindexed {
            self.not_from.clone()
        } else {
            vec![]
        };

        let matcher = Matcher {
            regex,
            case_sensitive: self.case_sensitive,
            groups,
            excluded,
            not_from,
        };

        (matcher.regex.is_some()
            || !matcher.groups.is_empty()
            || !matcher.excluded.is_empty()
            || !matcher.not_from.is_empty())
        .then_some(matcher)
    }

    fn has_positive(&self) -> bool {
        !self.from.is_empty() || !self.groups.is_empty()
    }

    pub fn fts(&self) -> Option<String> {
        if !self.has_positive() {
            return None;
        }

        let mut positive = vec![];

        if !self.from.is_empty() {
            positive.push(nick_expression(&self.from));
        }

        for group in &self.groups {
            let terms = group.iter().map(text_expression).collect::<Vec<_>>();
            positive.push(if terms.len() == 1 {
                terms.into_iter().next().unwrap_or_default()
            } else {
                format!("({})", terms.join(" OR "))
            });
        }

        let fts = positive.join(" AND ");

        // Case sensitive exclusions are checked by the matcher instead.
        let excluded = if self.case_sensitive {
            &[]
        } else {
            self.excluded.as_slice()
        };
        let mut negative =
            excluded.iter().map(text_expression).collect::<Vec<_>>();
        if !self.not_from.is_empty() {
            negative.push(nick_expression(&self.not_from));
        }

        Some(if negative.is_empty() {
            fts
        } else {
            format!("({fts}) NOT ({})", negative.join(" OR "))
        })
    }

    pub fn matcher(&self) -> Option<&Matcher> {
        self.matcher.as_ref()
    }

    /// Whether `server:` lets through a history's server, as `{server:b}`.
    pub(super) fn matches_server(&self, server: &str) -> bool {
        let (name, network) = match server.split_once('\0') {
            Some((name, network)) => (name, Some(network)),
            None => (server, None),
        };

        // Names may hold slashes too, so compare whole exact values.
        let exact = format!("{name}/{}", network.unwrap_or_default());

        self.servers.is_empty()
            || self.servers.iter().any(|value| {
                exact.eq_ignore_ascii_case(value)
                    || name.eq_ignore_ascii_case(value)
                    || network.is_some_and(|network| {
                        network.eq_ignore_ascii_case(value)
                    })
            })
    }

    pub(super) fn has_server_filter(&self) -> bool {
        !self.servers.is_empty()
    }

    pub fn is_runnable(&self) -> bool {
        let text_length: usize = self
            .groups
            .iter()
            .flatten()
            .map(|term| {
                term.text.chars().filter(|c| c.is_alphanumeric()).count()
            })
            .sum();

        self.errors.is_empty()
            && (!self.from.is_empty()
                || self.regex.is_some()
                || text_length >= MIN_TEXT_LENGTH)
    }

    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn terms(&self) -> impl Iterator<Item = &str> {
        self.groups.iter().flatten().map(|term| term.text.as_str())
    }
}

/// Post-filter for what the full-text index can't express.
#[derive(Debug, Clone)]
pub struct Matcher {
    regex: Option<Regex>,
    case_sensitive: bool,
    /// Every group needs one match.
    groups: Vec<Vec<Pattern>>,
    excluded: Vec<Pattern>,
    /// Lowercase nicks.
    not_from: Vec<String>,
}

impl Matcher {
    pub fn is_match(&self, text: &str, nick: Option<&str>) -> bool {
        if !self.not_from.is_empty()
            && let Some(nick) = nick
        {
            let nick = nick.to_lowercase();
            if self.not_from.contains(&nick) {
                return false;
            }
        }

        if !self.groups.is_empty() || !self.excluded.is_empty() {
            let tokens = tokens(text, self.case_sensitive);
            if !self
                .groups
                .iter()
                .all(|group| group.iter().any(|pattern| pattern.is_in(&tokens)))
                || self.excluded.iter().any(|pattern| pattern.is_in(&tokens))
            {
                return false;
            }
        }

        self.regex
            .as_ref()
            .is_none_or(|regex| regex.is_match(text).unwrap_or(false))
    }
}

/// A word or phrase as the tokens the full-text index sees.
#[derive(Debug, Clone)]
struct Pattern {
    tokens: Vec<String>,
    prefix: bool,
}

impl Pattern {
    fn new(term: &Term, case_sensitive: bool) -> Self {
        Self {
            tokens: tokens(&term.text, case_sensitive),
            prefix: term.prefix,
        }
    }

    fn is_in(&self, tokens: &[String]) -> bool {
        let Some((last, rest)) = self.tokens.split_last() else {
            return false;
        };

        tokens.windows(self.tokens.len()).any(|window| {
            window[..rest.len()] == *rest
                && if self.prefix {
                    window[rest.len()].starts_with(last.as_str())
                } else {
                    window[rest.len()] == *last
                }
        })
    }
}

/// Splits `text` like the index's `unicode61 remove_diacritics 2` tokenizer.
fn tokens(text: &str, case_sensitive: bool) -> Vec<String> {
    let folded = text
        .nfd()
        .filter(|c| !is_combining_mark(*c))
        .collect::<String>();

    folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| {
            if case_sensitive {
                token.to_owned()
            } else {
                token.to_lowercase()
            }
        })
        .collect()
}

fn text_expression(term: &Term) -> String {
    let mut expression =
        format!("text : \"{}\"", term.text.replace('"', "\"\""));
    if term.prefix {
        expression.push('*');
    }
    expression
}

fn nick_expression(nicks: &[String]) -> String {
    let nicks = nicks
        .iter()
        .map(|nick| format!("\"{}\"", nick_token(nick)))
        .collect::<Vec<_>>();
    if nicks.len() == 1 {
        format!("nick : {}", nicks[0])
    } else {
        format!("nick : ({})", nicks.join(" OR "))
    }
}

fn day_bounds<Tz: TimeZone>(
    value: &str,
    tz: &Tz,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    let start = |date: NaiveDate| {
        let midnight = date.and_hms_opt(0, 0, 0)?;
        // Some zones skip midnight when daylight saving time starts.
        [midnight, midnight + TimeDelta::hours(1)]
            .into_iter()
            .find_map(|time| tz.from_local_datetime(&time).earliest())
            .map(|time| time.with_timezone(&Utc))
    };
    Some((start(date)?, start(date.succ_opt()?)?))
}

pub(super) fn nick_token(nick: &str) -> String {
    nick.to_lowercase()
        .bytes()
        .fold(String::new(), |mut token, byte| {
            let _ = write!(token, "{byte:02x}");
            token
        })
}

fn has_text(term: &str) -> bool {
    term.chars().any(char::is_alphanumeric)
}

/// Replaces every `key:` filter in `input` with `key:value`, or removes them.
pub fn set_filter(input: &str, key: &str, value: Option<&str>) -> String {
    let filters = lex(input)
        .into_iter()
        .filter(|token| {
            matches!(&token.kind, TokenKind::Filter(found, _) if found.eq_ignore_ascii_case(key))
        })
        .map(|token| token.span)
        .collect::<Vec<_>>();

    let at = filters.first().map_or(0, |span| span.start);
    let mut rest = String::new();
    let mut cursor = 0;
    for span in &filters {
        rest.push_str(&input[cursor..span.start]);
        cursor = span.end;
        // Drop the separating whitespace along with the filter.
        cursor += input[cursor..].len() - input[cursor..].trim_start().len();
    }
    rest.push_str(&input[cursor..]);

    let Some(value) = value else {
        return rest;
    };

    let token = filter_token(key, value);
    let at = at.min(rest.len());
    let (head, tail) = rest.split_at(at);
    let tail = tail.trim_start();

    if tail.is_empty() {
        format!("{head}{token} ")
    } else {
        format!("{head}{token} {tail}")
    }
}

/// Removes every filter, keeping the text being searched for.
pub fn clear_filters(input: &str) -> String {
    FILTERS
        .iter()
        .fold(input.to_owned(), |input, key| set_filter(&input, key, None))
}

/// The filters in a query, lexed once.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    /// Lowercase keys and values of the filters that aren't negated or empty.
    values: Vec<(String, String)>,
    any: bool,
}

impl Filters {
    pub fn of(input: &str) -> Self {
        let mut filters = Self::default();

        for token in lex(input) {
            if let TokenKind::Filter(key, value) = token.kind {
                filters.any = true;
                if !token.negated && !value.is_empty() {
                    filters.values.push((key, value));
                }
            }
        }

        filters
    }

    /// Whether the query has any filters.
    pub fn any(&self) -> bool {
        self.any
    }

    /// The value of the last `key:` filter.
    pub fn value(&self, key: &str) -> Option<&str> {
        self.values(key).next_back()
    }

    /// The values of every `key:` filter.
    pub fn values<'a>(
        &'a self,
        key: &str,
    ) -> impl DoubleEndedIterator<Item = &'a str> {
        self.values
            .iter()
            .filter(move |(found, _)| found.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.as_str())
    }
}

/// A `server:` value for `server` among `servers`: its name, unless that's
/// also a bouncer network's name.
pub fn server_value<'a>(
    server: &Server,
    servers: impl IntoIterator<Item = &'a Server>,
) -> String {
    let ambiguous = server.network.is_some()
        || servers.into_iter().any(|other| {
            other.network.as_ref().is_some_and(|network| {
                network.name.eq_ignore_ascii_case(&server.name)
            })
        });

    if ambiguous {
        exact_server_value(server)
    } else {
        server.name.to_string()
    }
}

/// A `server:` value for only `server`: `bouncer/network` for a bouncer
/// network, and `name/` for a server without its bouncer networks.
pub fn exact_server_value(server: &Server) -> String {
    format!(
        "{}/{}",
        server.name,
        server
            .network
            .as_ref()
            .map_or("", |network| network.name.as_str())
    )
}

/// A query limited to one channel or query on one server.
pub fn scoped_query(server: &str, target: &str) -> String {
    format!(
        "{} {} ",
        filter_token("server", server),
        filter_token("in", target)
    )
}

fn filter_token(key: &str, value: &str) -> String {
    let plain = !value.is_empty()
        && !value.starts_with('"')
        && !value.contains(|c: char| c.is_whitespace() || c == '"');

    if plain {
        format!("{key}:{value}")
    } else {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        format!("{key}:\"{escaped}\"")
    }
}

#[derive(Debug)]
struct Token {
    span: Span<usize>,
    negated: bool,
    kind: TokenKind,
}

#[derive(Debug)]
enum TokenKind {
    Word(String),
    Phrase(String),
    Or,
    Regex(String),
    /// Lowercase key and unescaped value.
    Filter(String, String),
}

fn lex(input: &str) -> Vec<Token> {
    let mut tokens = vec![];
    let mut position = 0;

    loop {
        let rest = &input[position..];
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            break;
        }
        let start = position + rest.len() - trimmed.len();
        let mut at = start;

        let negated = input[at..].starts_with('-')
            && input[at + 1..]
                .chars()
                .next()
                .is_some_and(|c| !c.is_whitespace());
        if negated {
            at += 1;
        }

        let (kind, end) = if input[at..].starts_with('"') {
            let (phrase, end) = quoted(input, at + 1, false);
            (TokenKind::Phrase(phrase), end)
        } else if let Some((pattern, end)) = regex(input, at) {
            (TokenKind::Regex(pattern), end)
        } else {
            let length = input[at..]
                .find(|c: char| c.is_whitespace() || c == '"')
                .unwrap_or(input.len() - at);
            let end = at + length;
            let word = &input[at..end];

            match word.split_once(':') {
                Some((key, value))
                    if FILTERS
                        .iter()
                        .any(|filter| filter.eq_ignore_ascii_case(key)) =>
                {
                    let key = key.to_lowercase();
                    if value.is_empty() && input[end..].starts_with('"') {
                        let (value, end) = quoted(input, end + 1, true);
                        (TokenKind::Filter(key, value), end)
                    } else {
                        (TokenKind::Filter(key, value.to_owned()), end)
                    }
                }
                _ if word == "OR" && !negated => (TokenKind::Or, end),
                _ => (TokenKind::Word(word.to_owned()), end),
            }
        };

        tokens.push(Token {
            span: start..end,
            negated,
            kind,
        });
        position = end;
    }

    tokens
}

/// Reads up to the closing quote, which may be missing.
fn quoted(input: &str, start: usize, escapes: bool) -> (String, usize) {
    let mut text = String::new();
    let mut chars = input[start..].char_indices();

    while let Some((index, c)) = chars.next() {
        match c {
            '"' => return (text, start + index + 1),
            '\\' if escapes => {
                if let Some((_, escaped)) = chars.next() {
                    text.push(escaped);
                }
            }
            c => text.push(c),
        }
    }

    (text, input.len())
}

/// `/pattern/`, where `\/` is a slash and the closing slash ends a word, so
/// paths like `/etc/hosts` stay words; anything unterminated is a word.
fn regex(input: &str, start: usize) -> Option<(String, usize)> {
    let body = input[start..].strip_prefix('/')?;
    let mut pattern = String::new();
    let mut chars = body.char_indices();

    while let Some((index, c)) = chars.next() {
        match c {
            '/' if body[index + 1..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace) =>
            {
                return (!pattern.is_empty())
                    .then_some((pattern, start + 1 + index + 1));
            }
            '\\' => match chars.next() {
                Some((_, '/')) => pattern.push('/'),
                Some((_, escaped)) => {
                    pattern.push('\\');
                    pattern.push(escaped);
                }
                None => pattern.push('\\'),
            },
            c => pattern.push(c),
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;

    use super::*;

    fn parse(input: &str) -> Query {
        Query::parse_in(input, &Utc)
    }

    fn fts(input: &str) -> Option<String> {
        parse(input).fts()
    }

    #[test]
    fn words_are_quoted_and_last_is_prefix() {
        assert_eq!(
            fts("hello wor").as_deref(),
            Some("text : \"hello\" AND text : \"wor\"*")
        );
    }

    #[test]
    fn trailing_space_disables_prefix() {
        assert_eq!(
            fts("hello world ").as_deref(),
            Some("text : \"hello\" AND text : \"world\"")
        );
    }

    #[test]
    fn phrases() {
        assert_eq!(
            fts("\"exact phrase\" more").as_deref(),
            Some("text : \"exact phrase\" AND text : \"more\"*")
        );
        assert_eq!(fts("\"exact phr").as_deref(), Some("text : \"exact phr\""));
    }

    #[test]
    fn syntax_characters_are_escaped() {
        assert_eq!(
            fts("bar:baz a*b ^x (y) NOT").as_deref(),
            Some(
                "text : \"bar:baz\" AND text : \"a*b\" AND text : \"^x\" AND text : \"(y)\" AND text : \"NOT\"*"
            )
        );
        assert_eq!(
            fts("it\"s").as_deref(),
            Some("text : \"it\" AND text : \"s\"")
        );
        assert_eq!(
            fts("a\"\"\"b").as_deref(),
            Some("text : \"a\" AND text : \"b\"")
        );
    }

    #[test]
    fn terms_without_text_are_dropped() {
        assert_eq!(fts("- * \"\" foo").as_deref(), Some("text : \"foo\"*"));
        assert_eq!(fts("- *"), None);
    }

    #[test]
    fn exclusions() {
        assert_eq!(
            fts("release -beta -\"rc 1\" ").as_deref(),
            Some(
                "(text : \"release\") NOT (text : \"beta\" OR text : \"rc 1\")"
            )
        );
        assert_eq!(
            fts("release -from:Bot").as_deref(),
            Some("(text : \"release\") NOT (nick : \"626f74\")")
        );
        // Only exclusions can't run: FTS needs something to match.
        assert_eq!(fts("-beta"), None);
        assert!(!parse("-beta").is_runnable());
        // With only a regex, the matcher checks the exclusions.
        let matcher = parse("/x/ -beta -from:Bot").matcher().cloned().unwrap();
        assert!(matcher.is_match("x", Some("casper")));
        assert!(!matcher.is_match("x Béta", Some("casper")));
        assert!(!matcher.is_match("x", Some("BOT")));
        // Otherwise FTS does.
        assert!(parse("ab -beta -from:Bot").matcher().is_none());
        // A lone dash is not an exclusion.
        assert_eq!(
            fts("a - b").as_deref(),
            Some("text : \"a\" AND text : \"b\"*")
        );
    }

    #[test]
    fn or_binds_tighter_than_and() {
        assert_eq!(
            fts("error OR warning disk").as_deref(),
            Some(
                "(text : \"error\" OR text : \"warning\") AND text : \"disk\"*"
            )
        );
        assert_eq!(
            fts("a OR b OR \"c d\"").as_deref(),
            Some("(text : \"a\" OR text : \"b\" OR text : \"c d\")")
        );
        // Dangling or lowercase operators.
        assert_eq!(fts("OR a OR").as_deref(), Some("text : \"a\""));
        assert_eq!(
            fts("a or b").as_deref(),
            Some("text : \"a\" AND text : \"or\" AND text : \"b\"*")
        );
        assert_eq!(
            fts("a OR -b c").as_deref(),
            Some("(text : \"a\" AND text : \"c\"*) NOT (text : \"b\")")
        );
    }

    #[test]
    fn filters() {
        let query = parse("From:Casper in:#Halloy release");
        assert_eq!(query.from, ["casper"]);
        assert_eq!(query.within, ["#Halloy"]);
        assert_eq!(
            query.fts().as_deref(),
            Some("nick : \"636173706572\" AND text : \"release\"*")
        );

        assert_eq!(
            fts("from:casper").as_deref(),
            Some("nick : \"636173706572\"")
        );
        // Empty filters are ignored.
        let query = parse("from: in:");
        assert!(query.fts().is_none() && query.errors().is_empty());
        assert!(query.within.is_empty());
    }

    #[test]
    fn repeated_filters_are_alternatives() {
        let query =
            parse("from:a from:b in:#x in:#y server:libera server:\"my net\"");
        assert_eq!(query.fts().as_deref(), Some("nick : (\"61\" OR \"62\")"));
        assert_eq!(query.within, ["#x", "#y"]);
        assert_eq!(query.servers, ["libera", "my net"]);

        let query = parse("server:\"say \\\"hi\\\"\" x");
        assert_eq!(query.servers, ["say \"hi\""]);
    }

    #[test]
    fn buffer_kind() {
        assert_eq!(parse("is:channel").kind, Some(BufferKind::Channel));
        assert_eq!(parse("IS:Query").kind, Some(BufferKind::Query));
        assert_eq!(parse("is:nothing ab").errors().len(), 1);
        assert!(!parse("is:nothing ab").is_runnable());
    }

    #[test]
    fn dates_use_local_days() {
        let utc =
            |text: &str| DateTime::parse_from_rfc3339(text).unwrap().to_utc();
        let tz = FixedOffset::west_opt(3 * 3600).unwrap();

        let query = Query::parse_in("after:2024-03-10 x", &tz);
        assert_eq!(query.after, Some(utc("2024-03-10T03:00:00Z")));
        assert_eq!(query.before, None);

        let query = Query::parse_in("before:2024-03-10", &tz);
        assert_eq!(query.before, Some(utc("2024-03-10T03:00:00Z")));

        let query = Query::parse_in("on:2024-02-29", &tz);
        assert_eq!(query.after, Some(utc("2024-02-29T03:00:00Z")));
        assert_eq!(query.before, Some(utc("2024-03-01T03:00:00Z")));

        // Combined dates narrow the range.
        let query = Query::parse_in(
            "after:2024-01-01 after:2024-02-01 before:2024-05-01 before:2024-04-01",
            &Utc,
        );
        assert_eq!(query.after, Some(utc("2024-02-01T00:00:00Z")));
        assert_eq!(query.before, Some(utc("2024-04-01T00:00:00Z")));

        let query = parse("after:2024-13-01 ab");
        assert_eq!(query.errors().len(), 1);
        assert!(!query.is_runnable());
    }

    #[test]
    fn dates_in_skipped_midnight() {
        // A zone change can skip local midnight; the day starts an hour later.
        let start = day_bounds("2024-01-01", &SkipsMidnight).unwrap().0;
        assert_eq!(
            start,
            DateTime::parse_from_rfc3339("2024-01-01T01:00:00Z")
                .unwrap()
                .to_utc()
        );
    }

    #[derive(Clone)]
    struct SkipsMidnight;

    impl TimeZone for SkipsMidnight {
        type Offset = FixedOffset;

        fn from_offset(_: &FixedOffset) -> Self {
            SkipsMidnight
        }

        fn offset_from_local_date(
            &self,
            _: &NaiveDate,
        ) -> chrono::LocalResult<FixedOffset> {
            chrono::LocalResult::Single(FixedOffset::east_opt(0).unwrap())
        }

        fn offset_from_local_datetime(
            &self,
            local: &chrono::NaiveDateTime,
        ) -> chrono::LocalResult<FixedOffset> {
            use chrono::Timelike;
            if local.hour() == 0 {
                chrono::LocalResult::None
            } else {
                chrono::LocalResult::Single(FixedOffset::east_opt(0).unwrap())
            }
        }

        fn offset_from_utc_date(&self, _: &NaiveDate) -> FixedOffset {
            FixedOffset::east_opt(0).unwrap()
        }

        fn offset_from_utc_datetime(
            &self,
            _: &chrono::NaiveDateTime,
        ) -> FixedOffset {
            FixedOffset::east_opt(0).unwrap()
        }
    }

    #[test]
    fn regex() {
        let query = parse("/err(or)?/ disk");
        assert!(query.is_runnable());
        assert_eq!(query.fts().as_deref(), Some("text : \"disk\"*"));
        let matcher = query.matcher().cloned().unwrap();
        assert!(matcher.is_match("ERROR on disk", None));
        assert!(!matcher.is_match("all fine", None));

        let query = parse("/a\\/b/");
        assert!(query.is_runnable());
        assert_eq!(query.fts(), None);
        assert!(query.matcher().cloned().unwrap().is_match("x a/b y", None));

        // Spaces are part of the pattern.
        assert!(
            parse("/a b/")
                .matcher()
                .cloned()
                .unwrap()
                .is_match("a b", None)
        );

        // Unterminated: a plain word.
        assert_eq!(fts("/join").as_deref(), Some("text : \"/join\"*"));
        // The closing slash ends a word, so paths and commands stay words.
        assert_eq!(
            fts("/etc/hosts").as_deref(),
            Some("text : \"/etc/hosts\"*")
        );
        assert_eq!(
            fts("/join and /part").as_deref(),
            Some("text : \"/join\" AND text : \"and\" AND text : \"/part\"*")
        );
        assert!(
            parse("/a/b/ c")
                .matcher()
                .cloned()
                .unwrap()
                .is_match("a/b", None)
        );

        let query = parse("/(/");
        assert_eq!(query.errors().len(), 1);
        assert!(!query.is_runnable());
        assert_eq!(parse("/a/ /b/").errors().len(), 1);
    }

    #[test]
    fn case_sensitive() {
        assert!(parse("Halloy").matcher().is_none());

        let query = parse("case:sensitive Halloy rel");
        let matcher = query.matcher().cloned().unwrap();
        assert!(matcher.is_match("Halloy release", None));
        assert!(!matcher.is_match("halloy release", None));
        assert!(!matcher.is_match("HalloyX release", None));
        assert!(!matcher.is_match("Halloy Release", None));

        let matcher = parse("case:sensitive Foo OR Bar -Baz")
            .matcher()
            .cloned()
            .unwrap();
        assert!(matcher.is_match("Bar here", None));
        assert!(!matcher.is_match("bar here", None));
        assert!(!matcher.is_match("Foo and Baz", None));
        assert!(matcher.is_match("Foo and baz", None));
        assert_eq!(
            parse("case:sensitive Foo -Baz").fts().as_deref(),
            Some("text : \"Foo\"")
        );

        let matcher = parse("case:sensitive /Err/").matcher().cloned().unwrap();
        assert!(matcher.is_match("Err", None));
        assert!(!matcher.is_match("err", None));

        assert!(parse("case:wrong ab").errors().len() == 1);
    }

    #[test]
    fn case_sensitive_matches_tokens_like_the_index() {
        // Punctuation, spacing and diacritics don't matter, only case.
        let matcher = parse("case:sensitive \"Foo Bar\" Cafe ")
            .matcher()
            .cloned()
            .unwrap();
        assert!(matcher.is_match("Foo-Bar at the Café", None));
        assert!(matcher.is_match("Foo  Bar\nCafe", None));
        assert!(!matcher.is_match("Foo bar Cafe", None));

        let matcher =
            parse("case:sensitive e-mail ").matcher().cloned().unwrap();
        assert!(matcher.is_match("send an e mail", None));

        // Overlapping candidates are all tried.
        let matcher = parse("case:sensitive a-a ").matcher().cloned().unwrap();
        assert!(matcher.is_match("xa-a-a", None));

        let matcher =
            parse("case:sensitive ab -Beta").matcher().cloned().unwrap();
        assert!(!matcher.is_match("ab Béta", None));
        assert!(matcher.is_match("ab beta", None));

        // Nothing to check after the index.
        assert!(parse("case:sensitive from:x").matcher().is_none());
    }

    #[test]
    fn exact_server_values_allow_slashes_in_names() {
        let query = parse("ab server:home/soju/OFTC");
        assert!(query.matches_server("home/soju\0OFTC"));
        assert!(!query.matches_server("home/soju\0libera"));

        let query = parse("ab server:a/b/");
        assert!(query.matches_server("a/b"));
        assert!(!query.matches_server("a/b\0c"));

        // The bare name of a bouncer covers its networks.
        assert!(parse("ab server:home/soju").matches_server("home/soju\0OFTC"));
    }

    #[test]
    fn unsupported_negations() {
        assert_eq!(parse("-in:#a ab").errors().len(), 1);
        assert_eq!(parse("-/a/ ab").errors().len(), 1);
    }

    #[test]
    fn runnable() {
        assert!(!parse("").is_runnable());
        assert!(!parse("a").is_runnable());
        assert!(!parse("- -").is_runnable());
        assert!(parse("ab").is_runnable());
        assert!(!parse("in:#halloy").is_runnable());
        assert!(
            !parse("server:libera is:channel after:2024-01-01").is_runnable()
        );
        assert!(parse("from:casper").is_runnable());
        assert!(parse("/x/").is_runnable());
        assert!(!parse("-from:casper").is_runnable());
    }

    #[test]
    fn set_and_read_filters() {
        assert_eq!(
            set_filter("hello wor", "server", Some("libera")),
            "server:libera hello wor"
        );
        assert_eq!(set_filter("", "is", Some("channel")), "is:channel ");
        assert_eq!(
            set_filter(
                "a server:x b server:\"y z\" c",
                "server",
                Some("my net")
            ),
            "a server:\"my net\" b c"
        );
        assert_eq!(set_filter("a server:x b", "server", None), "a b");
        assert_eq!(set_filter("a SERVER:x", "server", None), "a ");
        assert_eq!(set_filter("server:x", "server", None), "");
        assert_eq!(set_filter("case:sensitive ab", "case", None), "ab");
        // Words that only look like filters are left alone.
        assert_eq!(set_filter("serverx:1 ab", "server", None), "serverx:1 ab");

        let filters = Filters::of("a server:x server:\"y z\"");
        assert_eq!(filters.value("server"), Some("y z"));
        assert_eq!(
            Filters::of("a after:2024-01-01").value("AFTER"),
            Some("2024-01-01")
        );
        assert_eq!(Filters::of("a -from:x").value("from"), None);
        assert_eq!(Filters::of("a server:").value("server"), None);
        assert_eq!(
            Filters::of("server:x a server: -server:y SERVER:z")
                .values("server")
                .collect::<Vec<_>>(),
            ["x", "z"]
        );

        assert_eq!(
            clear_filters("server:x hello -from:y \"in:z\" case:sensitive wor"),
            "hello \"in:z\" wor"
        );
        assert!(Filters::of("a is:channel").any());
        assert!(Filters::of("a -from:x").any());
        assert!(!Filters::of("a \"is:channel\" b:c").any());
    }

    #[test]
    fn scoped() {
        assert_eq!(
            scoped_query("libera", "#halloy"),
            "server:libera in:#halloy "
        );
        assert_eq!(
            scoped_query("My Net", "#a\"b"),
            "server:\"My Net\" in:\"#a\\\"b\" "
        );

        let query = parse(&scoped_query("My \"Net\"", "#a\"b"));
        assert_eq!(query.servers, ["My \"Net\""]);
        assert_eq!(query.within, ["#a\"b"]);
        assert!(query.errors().is_empty());
    }
}
