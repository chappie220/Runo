//! A small syntax highlighter for code blocks in replies: comments, strings,
//! numbers, keywords, types and calls, for the languages models write most.
//! It works line by line and only tracks block comments across lines, which
//! is enough to read code at a glance without a grammar engine.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

const KEYWORD: Color = Color::Magenta;
const STRING: Color = Color::Green;
const NUMBER: Color = Color::LightYellow;
const COMMENT: Color = Color::DarkGray;
const TYPE: Color = Color::Yellow;
const CALL: Color = Color::LightBlue;
/// Keys in TOML, YAML and JSON.
const KEY: Color = Color::Cyan;

/// What one language looks like to the highlighter.
struct Syntax {
    keywords: &'static [&'static str],
    line_comment: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    /// `'` starts a string (not a Rust lifetime or char).
    single_quote: bool,
    backtick: bool,
    /// `key:` / `key =` at the start of a line is a key (config formats).
    keys: bool,
}

const RUST: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];
const PYTHON: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "self", "True", "try",
    "while", "with", "yield",
];
const JS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "of",
    "return",
    "static",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "yield",
];
const GO: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "false",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "nil",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "true",
    "type",
    "var",
];
const C: &[&str] = &[
    "auto",
    "bool",
    "break",
    "case",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "extern",
    "false",
    "final",
    "float",
    "for",
    "if",
    "implements",
    "import",
    "int",
    "long",
    "namespace",
    "new",
    "null",
    "nullptr",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "template",
    "this",
    "throw",
    "true",
    "try",
    "typedef",
    "unsigned",
    "using",
    "virtual",
    "void",
    "while",
];
const SHELL: &[&str] = &[
    "case", "do", "done", "echo", "elif", "else", "esac", "exit", "export", "fi", "for",
    "function", "if", "in", "local", "return", "set", "then", "unset", "until", "while",
];
const SQL: &[&str] = &[
    "and", "as", "by", "create", "delete", "desc", "drop", "from", "group", "insert", "into",
    "join", "left", "limit", "not", "null", "on", "or", "order", "primary", "key", "select", "set",
    "table", "update", "values", "where",
];
const CONFIG: &[&str] = &["true", "false", "null", "yes", "no"];

fn syntax(lang: &str) -> Option<Syntax> {
    let base = Syntax {
        keywords: &[],
        line_comment: &["//"],
        block_comment: Some(("/*", "*/")),
        single_quote: true,
        backtick: false,
        keys: false,
    };
    Some(match lang.trim().to_lowercase().as_str() {
        "rust" | "rs" => Syntax {
            keywords: RUST,
            single_quote: false,
            ..base
        },
        "python" | "py" => Syntax {
            keywords: PYTHON,
            line_comment: &["#"],
            block_comment: None,
            ..base
        },
        "javascript" | "js" | "jsx" | "typescript" | "ts" | "tsx" | "mjs" => Syntax {
            keywords: JS,
            backtick: true,
            ..base
        },
        "go" | "golang" => Syntax {
            keywords: GO,
            backtick: true,
            ..base
        },
        "c" | "h" | "cpp" | "c++" | "cc" | "hpp" | "java" | "kotlin" | "kt" | "cs" | "csharp"
        | "swift" => Syntax {
            keywords: C,
            ..base
        },
        "sh" | "bash" | "zsh" | "fish" | "shell" | "console" => Syntax {
            keywords: SHELL,
            line_comment: &["#"],
            block_comment: None,
            ..base
        },
        "sql" => Syntax {
            keywords: SQL,
            line_comment: &["--"],
            ..base
        },
        "toml" | "ini" | "yaml" | "yml" | "json" | "jsonc" => Syntax {
            keywords: CONFIG,
            line_comment: &["#", "//"],
            block_comment: None,
            keys: true,
            ..base
        },
        _ => return None,
    })
}

/// Carries a block comment from one line to the next.
#[derive(Default)]
pub struct State {
    in_block: bool,
}

/// One line of a code block in `lang` as styled spans; plain for languages
/// it does not know.
pub fn line(text: &str, lang: &str, state: &mut State) -> Vec<Span<'static>> {
    let Some(syntax) = syntax(lang) else {
        return vec![Span::raw(text.to_owned())];
    };
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let flush = |plain: &mut String, spans: &mut Vec<Span<'static>>| {
        if !plain.is_empty() {
            spans.push(Span::raw(std::mem::take(plain)));
        }
    };
    let starts = |i: usize, s: &str| {
        s.chars()
            .enumerate()
            .all(|(k, c)| chars.get(i + k) == Some(&c))
    };
    let take = |from: usize, to: usize| chars[from..to.min(chars.len())].iter().collect::<String>();
    let mut i = 0;
    if syntax.keys {
        // `key =`, `key:` and `"key":` up to the separator.
        let rest = text.trim_start();
        let indent = chars.len() - rest.chars().count();
        let end = rest
            .char_indices()
            .find(|&(_, c)| c == '=' || c == ':')
            .map(|(b, _)| rest[..b].chars().count());
        if let Some(end) = end
            && end > 0
            && !rest.starts_with('#')
            && !rest.starts_with('[')
            && !rest.starts_with('-')
            && !rest.starts_with("//")
        {
            plain.push_str(&take(0, indent));
            flush(&mut plain, &mut spans);
            spans.push(Span::styled(
                take(indent, indent + end),
                Style::new().fg(KEY),
            ));
            i = indent + end;
        }
    }
    while i < chars.len() {
        if state.in_block {
            let close = syntax.block_comment.map_or("*/", |(_, close)| close);
            let end = (i..chars.len())
                .find(|&j| starts(j, close))
                .map_or(chars.len(), |j| {
                    state.in_block = false;
                    j + close.chars().count()
                });
            flush(&mut plain, &mut spans);
            spans.push(comment(take(i, end)));
            i = end;
            continue;
        }
        let c = chars[i];
        if let Some((open, _)) = syntax.block_comment
            && starts(i, open)
        {
            state.in_block = true;
            flush(&mut plain, &mut spans);
            spans.push(comment(open.to_owned()));
            i += open.chars().count();
            continue;
        }
        if syntax.line_comment.iter().any(|m| starts(i, m))
            // `#` only starts a comment at a word boundary (not `a#b` or `$#`).
            && (c != '#' || i == 0 || chars[i - 1].is_whitespace())
        {
            flush(&mut plain, &mut spans);
            spans.push(comment(take(i, chars.len())));
            break;
        }
        let quote = c == '"' || (c == '\'' && syntax.single_quote) || (c == '`' && syntax.backtick);
        if quote {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != c {
                j += if chars[j] == '\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(chars.len());
            flush(&mut plain, &mut spans);
            spans.push(Span::styled(take(i, end), Style::new().fg(STRING)));
            i = end;
            continue;
        }
        let boundary = i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
        if c.is_ascii_digit() && boundary {
            let end = (i..chars.len())
                .find(|&j| {
                    !(chars[j].is_ascii_alphanumeric() || chars[j] == '.' || chars[j] == '_')
                })
                .unwrap_or(chars.len());
            flush(&mut plain, &mut spans);
            spans.push(Span::styled(take(i, end), Style::new().fg(NUMBER)));
            i = end;
            continue;
        }
        if (c.is_alphabetic() || c == '_') && boundary {
            let end = (i..chars.len())
                .find(|&j| !(chars[j].is_alphanumeric() || chars[j] == '_'))
                .unwrap_or(chars.len());
            let word = take(i, end);
            let keyword = syntax
                .keywords
                .iter()
                .any(|k| *k == word || (syntax.keywords == SQL && k.eq_ignore_ascii_case(&word)));
            let style = if keyword {
                Some(Style::new().fg(KEYWORD).add_modifier(Modifier::BOLD))
            } else if chars.get(end) == Some(&'(') || chars.get(end) == Some(&'!') {
                Some(Style::new().fg(CALL))
            } else if word.starts_with(|c: char| c.is_uppercase()) && syntax.keywords != SHELL {
                Some(Style::new().fg(TYPE))
            } else {
                None
            };
            match style {
                Some(style) => {
                    flush(&mut plain, &mut spans);
                    spans.push(Span::styled(word, style));
                }
                None => plain.push_str(&word),
            }
            i = end;
            continue;
        }
        plain.push(c);
        i += 1;
    }
    flush(&mut plain, &mut spans);
    spans
}

fn comment(text: String) -> Span<'static> {
    Span::styled(
        text,
        Style::new().fg(COMMENT).add_modifier(Modifier::ITALIC),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colored(spans: &[Span]) -> Vec<(String, Option<Color>)> {
        spans
            .iter()
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect()
    }

    #[test]
    fn colors_keywords_strings_numbers_and_comments() {
        let mut state = State::default();
        let spans = line(r#"let x = foo("a\"b", 42); // done"#, "rust", &mut state);
        assert_eq!(
            colored(&spans),
            [
                ("let".into(), Some(KEYWORD)),
                (" x = ".into(), None),
                ("foo".into(), Some(CALL)),
                ("(".into(), None),
                (r#""a\"b""#.into(), Some(STRING)),
                (", ".into(), None),
                ("42".into(), Some(NUMBER)),
                ("); ".into(), None),
                ("// done".into(), Some(COMMENT)),
            ]
        );
        // Text is never lost or reordered.
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, r#"let x = foo("a\"b", 42); // done"#);
    }

    #[test]
    fn block_comments_span_lines_and_unknown_languages_stay_plain() {
        let mut state = State::default();
        line("a /* start", "c", &mut state);
        assert_eq!(
            colored(&line("still */ int", "c", &mut state))[0].1,
            Some(COMMENT)
        );
        assert!(!state.in_block);
        assert_eq!(
            colored(&line("x = 1", "brainfuck", &mut state)),
            [("x = 1".into(), None)]
        );
        let toml = line("name = \"x\" # note", "toml", &mut State::default());
        assert_eq!(toml[0].style.fg, Some(KEY));
        assert_eq!(toml.last().unwrap().style.fg, Some(COMMENT));
        // `$#` in shell is not a comment.
        let sh = line("echo $# # count", "bash", &mut State::default());
        assert_eq!(sh.last().unwrap().content, "# count");
    }
}
