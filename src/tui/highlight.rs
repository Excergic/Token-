//! Code in an answer, split into token kinds. A lexer per family rather than
//! a grammar per language: it only has to colour what a reader scans for
//! (keywords, strings, comments, calls), and a wrong guess costs a colour,
//! never a character.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tok {
    Plain,
    Keyword,
    Type,
    Str,
    Comment,
    Number,
    Function,
    Macro,
    Constant,
    Punct,
    Attr,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Code,
    Shell,
    Json,
    Config,
    Markup,
}

struct Lang {
    family: Family,
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    constants: &'static [&'static str],
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    triple_quotes: bool,
    bang_macros: bool,
    /// Rust `'a`: a quote that starts a lifetime rather than a char.
    lifetimes: bool,
}

const RUST: Lang = Lang {
    family: Family::Code,
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
        "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "type",
        "unsafe", "use", "where", "while",
    ],
    types: &[
        "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
        "f32", "f64", "bool", "char", "str",
    ],
    constants: &["true", "false", "None", "Some", "Ok", "Err"],
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    triple_quotes: false,
    bang_macros: true,
    lifetimes: true,
};

const PYTHON: Lang = Lang {
    family: Family::Code,
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is",
        "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with",
        "yield", "match", "case", "self",
    ],
    types: &[
        "int", "str", "float", "bool", "list", "dict", "set", "tuple", "bytes", "object",
    ],
    constants: &["True", "False", "None"],
    line_comments: &["#"],
    block_comment: None,
    triple_quotes: true,
    bang_macros: false,
    lifetimes: false,
};

const JAVASCRIPT: Lang = Lang {
    family: Family::Code,
    keywords: &[
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "debugger",
        "default",
        "delete",
        "do",
        "else",
        "export",
        "extends",
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
        "of",
        "return",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "try",
        "type",
        "typeof",
        "var",
        "void",
        "while",
        "with",
        "yield",
        "enum",
        "implements",
        "private",
        "public",
        "protected",
        "readonly",
        "as",
    ],
    types: &[
        "string", "number", "boolean", "any", "unknown", "never", "object", "symbol", "bigint",
    ],
    constants: &["true", "false", "null", "undefined", "NaN", "Infinity"],
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    triple_quotes: false,
    bang_macros: false,
    lifetimes: false,
};

const GO: Lang = Lang {
    family: Family::Code,
    keywords: &[
        "break",
        "case",
        "chan",
        "const",
        "continue",
        "default",
        "defer",
        "else",
        "fallthrough",
        "for",
        "func",
        "go",
        "goto",
        "if",
        "import",
        "interface",
        "map",
        "package",
        "range",
        "return",
        "select",
        "struct",
        "switch",
        "type",
        "var",
    ],
    types: &[
        "int", "int8", "int16", "int32", "int64", "uint", "uint8", "uint16", "uint32", "uint64",
        "float32", "float64", "string", "bool", "byte", "rune", "error", "any",
    ],
    constants: &["true", "false", "nil", "iota"],
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    triple_quotes: false,
    bang_macros: false,
    lifetimes: false,
};

const CLIKE: Lang = Lang {
    family: Family::Code,
    keywords: &[
        "auto",
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
        "enum",
        "extends",
        "extern",
        "final",
        "finally",
        "for",
        "fun",
        "func",
        "goto",
        "if",
        "implements",
        "import",
        "include",
        "inline",
        "interface",
        "let",
        "namespace",
        "new",
        "override",
        "package",
        "private",
        "protected",
        "public",
        "return",
        "sizeof",
        "static",
        "struct",
        "switch",
        "template",
        "this",
        "throw",
        "throws",
        "try",
        "typedef",
        "typename",
        "union",
        "using",
        "val",
        "var",
        "virtual",
        "void",
        "volatile",
        "while",
        "define",
        "ifdef",
        "ifndef",
        "endif",
        "guard",
        "self",
    ],
    types: &[
        "int", "long", "short", "char", "float", "double", "bool", "boolean", "byte", "unsigned",
        "signed", "size_t", "string", "String", "Int", "Double",
    ],
    constants: &["true", "false", "null", "nullptr", "NULL", "nil"],
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    triple_quotes: false,
    bang_macros: false,
    lifetimes: false,
};

const SHELL: Lang = Lang {
    family: Family::Shell,
    keywords: &[
        "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
        "in", "function", "return", "export", "local", "readonly",
    ],
    types: &[],
    constants: &["true", "false"],
    line_comments: &["#"],
    block_comment: None,
    triple_quotes: false,
    bang_macros: false,
    lifetimes: false,
};

const SQL: Lang = Lang {
    family: Family::Code,
    keywords: &[
        "select",
        "from",
        "where",
        "insert",
        "into",
        "values",
        "update",
        "set",
        "delete",
        "create",
        "table",
        "drop",
        "alter",
        "add",
        "index",
        "join",
        "left",
        "right",
        "inner",
        "outer",
        "on",
        "and",
        "or",
        "not",
        "null",
        "is",
        "in",
        "as",
        "group",
        "by",
        "order",
        "having",
        "limit",
        "offset",
        "primary",
        "key",
        "foreign",
        "references",
        "distinct",
        "union",
        "all",
        "exists",
        "case",
        "when",
        "then",
        "else",
        "end",
        "default",
        "unique",
    ],
    types: &[
        "integer",
        "int",
        "text",
        "varchar",
        "boolean",
        "real",
        "blob",
        "timestamp",
        "date",
        "serial",
        "bigint",
        "numeric",
        "json",
        "jsonb",
        "uuid",
    ],
    constants: &["true", "false"],
    line_comments: &["--"],
    block_comment: Some(("/*", "*/")),
    triple_quotes: false,
    bang_macros: false,
    lifetimes: false,
};

const GENERIC: Lang = Lang {
    family: Family::Code,
    keywords: &[
        "fn", "def", "function", "func", "let", "var", "const", "if", "else", "for", "while",
        "return", "class", "struct", "import", "from", "use", "pub", "new", "match", "case",
        "switch", "try", "catch", "end", "do", "then", "module", "require",
    ],
    types: &[],
    constants: &["true", "false", "null", "nil", "None", "True", "False"],
    line_comments: &["//", "#"],
    block_comment: Some(("/*", "*/")),
    triple_quotes: false,
    bang_macros: false,
    lifetimes: false,
};

const JSON: Lang = Lang {
    family: Family::Json,
    keywords: &[],
    types: &[],
    constants: &["true", "false", "null"],
    line_comments: &["//"],
    block_comment: None,
    triple_quotes: false,
    bang_macros: false,
    lifetimes: false,
};

const CONFIG: Lang = Lang {
    family: Family::Config,
    keywords: &[],
    types: &[],
    constants: &["true", "false", "null", "yes", "no", "on", "off"],
    line_comments: &["#", ";"],
    block_comment: None,
    triple_quotes: true,
    bang_macros: false,
    lifetimes: false,
};

const MARKUP: Lang = Lang {
    family: Family::Markup,
    keywords: &[],
    types: &[],
    constants: &[],
    line_comments: &[],
    block_comment: Some(("<!--", "-->")),
    triple_quotes: false,
    bang_macros: false,
    lifetimes: false,
};

fn lang_for(tag: &str) -> &'static Lang {
    let tag = tag
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == '{')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match tag.as_str() {
        "rust" | "rs" => &RUST,
        "python" | "py" | "python3" => &PYTHON,
        "js" | "javascript" | "jsx" | "ts" | "typescript" | "tsx" | "mjs" | "cjs" => &JAVASCRIPT,
        "go" | "golang" => &GO,
        "c" | "h" | "cpp" | "c++" | "cc" | "hpp" | "java" | "cs" | "csharp" | "kotlin" | "kt"
        | "swift" | "scala" | "dart" | "zig" | "php" => &CLIKE,
        "sh" | "bash" | "zsh" | "shell" | "console" | "fish" | "shellsession" | "terminal" => {
            &SHELL
        }
        "sql" | "sqlite" | "postgres" | "postgresql" | "mysql" => &SQL,
        "json" | "jsonc" | "json5" | "jsonl" => &JSON,
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "env" | "dotenv" | "properties" => {
            &CONFIG
        }
        "html" | "xml" | "svg" | "vue" | "svelte" => &MARKUP,
        _ => &GENERIC,
    }
}

/// The body of a fenced block, one token list per line.
pub fn highlight(tag: &str, body: &str) -> Vec<Vec<(Tok, String)>> {
    let lang = lang_for(tag);
    let chars: Vec<char> = body.chars().collect();
    let tokens = match lang.family {
        Family::Markup => lex_markup(&chars),
        _ => lex(lang, &chars),
    };
    split_lines(tokens)
}

fn split_lines(tokens: Vec<(Tok, String)>) -> Vec<Vec<(Tok, String)>> {
    let mut lines: Vec<Vec<(Tok, String)>> = vec![Vec::new()];
    for (tok, text) in tokens {
        for (index, piece) in text.split('\n').enumerate() {
            if index > 0 {
                lines.push(Vec::new());
            }
            if piece.is_empty() {
                continue;
            }
            let line = lines.last_mut().expect("never empty");
            match line.last_mut() {
                Some((last, have)) if *last == tok => have.push_str(piece),
                _ => line.push((tok, piece.to_string())),
            }
        }
    }
    lines
}

fn starts(chars: &[char], at: usize, needle: &str) -> bool {
    let mut index = at;
    for ch in needle.chars() {
        if chars.get(index) != Some(&ch) {
            return false;
        }
        index += 1;
    }
    true
}

fn until(chars: &[char], from: usize, needle: &str) -> usize {
    let mut index = from;
    while index < chars.len() {
        if starts(chars, index, needle) {
            return index + needle.chars().count();
        }
        index += 1;
    }
    chars.len()
}

fn line_end(chars: &[char], from: usize) -> usize {
    (from..chars.len())
        .find(|at| chars[*at] == '\n')
        .unwrap_or(chars.len())
}

fn string_end(chars: &[char], from: usize, quote: char, multiline: bool) -> usize {
    let mut index = from + 1;
    while index < chars.len() {
        match chars[index] {
            '\\' => index += 2,
            '\n' if !multiline => return index,
            ch if ch == quote => return index + 1,
            _ => index += 1,
        }
    }
    chars.len()
}

fn is_ident_start(ch: char) -> bool {
    ch.is_alphabetic() || ch == '_'
}

fn is_ident(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn next_non_space(chars: &[char], from: usize) -> Option<char> {
    chars[from..]
        .iter()
        .find(|ch| **ch != ' ' && **ch != '\t')
        .copied()
}

fn at_line_start(chars: &[char], at: usize) -> bool {
    chars[..at]
        .iter()
        .rev()
        .take_while(|ch| **ch != '\n')
        .all(|ch| ch.is_whitespace())
}

fn lex(lang: &Lang, chars: &[char]) -> Vec<(Tok, String)> {
    let mut out: Vec<(Tok, String)> = Vec::new();
    let mut index = 0;
    // Shell: the first word of a command is the command.
    let mut command_position = true;
    let text = |from: usize, to: usize| chars[from..to].iter().collect::<String>();
    let emit = |out: &mut Vec<(Tok, String)>, tok: Tok, piece: String| match out.last_mut() {
        Some((last, have)) if *last == tok => have.push_str(&piece),
        _ => out.push((tok, piece)),
    };

    while index < chars.len() {
        let ch = chars[index];

        if let Some((open, close)) = lang.block_comment {
            if starts(chars, index, open) {
                let end = until(chars, index + open.chars().count(), close);
                emit(&mut out, Tok::Comment, text(index, end));
                index = end;
                continue;
            }
        }
        let comment = lang.line_comments.iter().find(|marker| {
            starts(chars, index, marker)
                && (lang.family != Family::Shell || index == 0 || chars[index - 1].is_whitespace())
        });
        if comment.is_some() {
            let end = line_end(chars, index);
            emit(&mut out, Tok::Comment, text(index, end));
            index = end;
            continue;
        }

        if lang.triple_quotes && (starts(chars, index, "\"\"\"") || starts(chars, index, "'''")) {
            let fence: String = std::iter::repeat_n(ch, 3).collect();
            let end = until(chars, index + 3, &fence);
            emit(&mut out, Tok::Str, text(index, end));
            index = end;
            continue;
        }

        if ch == '\'' && lang.lifetimes {
            let is_char =
                chars.get(index + 1) == Some(&'\\') || chars.get(index + 2) == Some(&'\'');
            if !is_char
                && chars
                    .get(index + 1)
                    .is_some_and(|next| is_ident_start(*next))
            {
                let mut end = index + 1;
                while end < chars.len() && is_ident(chars[end]) {
                    end += 1;
                }
                emit(&mut out, Tok::Attr, text(index, end));
                index = end;
                continue;
            }
        }

        if ch == '"' || ch == '\'' || (ch == '`' && lang.family == Family::Code) {
            let end = string_end(chars, index, ch, ch == '`');
            let is_key = matches!(lang.family, Family::Json | Family::Config)
                && next_non_space(chars, end) == Some(':');
            emit(
                &mut out,
                if is_key { Tok::Attr } else { Tok::Str },
                text(index, end),
            );
            command_position = false;
            index = end;
            continue;
        }

        if lang.family == Family::Config && ch == '[' && at_line_start(chars, index) {
            let end = line_end(chars, index);
            emit(&mut out, Tok::Type, text(index, end));
            index = end;
            continue;
        }

        if ch.is_ascii_digit() && (index == 0 || !is_ident(chars[index - 1])) {
            let mut end = index + 1;
            while end < chars.len()
                && (chars[end].is_ascii_alphanumeric()
                    || chars[end] == '_'
                    || (chars[end] == '.'
                        && chars.get(end + 1).is_some_and(|next| next.is_ascii_digit())))
            {
                end += 1;
            }
            emit(&mut out, Tok::Number, text(index, end));
            command_position = false;
            index = end;
            continue;
        }

        if lang.family == Family::Shell && ch == '$' {
            let mut end = index + 1;
            if chars.get(end) == Some(&'{') {
                end = until(chars, end, "}");
            } else {
                while end < chars.len() && (is_ident(chars[end]) || "?@#*!".contains(chars[end])) {
                    end += 1;
                    if !is_ident(chars[end - 1]) {
                        break;
                    }
                }
            }
            emit(&mut out, Tok::Constant, text(index, end));
            index = end;
            continue;
        }

        if lang.family == Family::Shell
            && ch == '-'
            && (index == 0 || chars[index - 1].is_whitespace())
            && !command_position
        {
            let mut end = index;
            while end < chars.len() && !chars[end].is_whitespace() && chars[end] != '=' {
                end += 1;
            }
            emit(&mut out, Tok::Attr, text(index, end));
            index = end;
            continue;
        }

        if (ch == '@' && lang.family == Family::Code)
            && chars
                .get(index + 1)
                .is_some_and(|next| is_ident_start(*next))
        {
            let mut end = index + 1;
            while end < chars.len() && (is_ident(chars[end]) || chars[end] == '.') {
                end += 1;
            }
            emit(&mut out, Tok::Attr, text(index, end));
            index = end;
            continue;
        }

        if ch == '#' && lang.bang_macros && matches!(chars.get(index + 1), Some('[') | Some('!')) {
            let end = until(chars, index, "]").min(line_end(chars, index));
            emit(&mut out, Tok::Attr, text(index, end));
            index = end;
            continue;
        }

        if is_ident_start(ch) {
            let mut end = index + 1;
            let dashed = matches!(lang.family, Family::Shell | Family::Config);
            while end < chars.len() && (is_ident(chars[end]) || (dashed && chars[end] == '-')) {
                end += 1;
            }
            let word = text(index, end);
            let lower = word.to_ascii_lowercase();
            let keyword_match = if std::ptr::eq(lang, &SQL) {
                lang.keywords.contains(&lower.as_str())
            } else {
                lang.keywords.contains(&word.as_str())
            };
            let next = next_non_space(chars, end);
            let tok = if lang.family == Family::Config
                && at_line_start(chars, index)
                && matches!(next, Some('=') | Some(':'))
            {
                Tok::Attr
            } else if keyword_match {
                Tok::Keyword
            } else if lang.constants.contains(&word.as_str()) {
                Tok::Constant
            } else if lang.family == Family::Shell && command_position {
                Tok::Function
            } else if lang.bang_macros
                && chars.get(end) == Some(&'!')
                && chars.get(end + 1) != Some(&'=')
            {
                end += 1;
                Tok::Macro
            } else if lang.types.contains(&word.as_str())
                || (std::ptr::eq(lang, &SQL) && lang.types.contains(&lower.as_str()))
            {
                Tok::Type
            } else if matches!(lang.family, Family::Code) && next == Some('(') {
                Tok::Function
            } else if matches!(lang.family, Family::Code)
                && word.len() > 1
                && word
                    .chars()
                    .all(|ch| ch.is_uppercase() || ch.is_ascii_digit() || ch == '_')
            {
                Tok::Constant
            } else if matches!(lang.family, Family::Code)
                && word.chars().next().is_some_and(char::is_uppercase)
            {
                Tok::Type
            } else {
                Tok::Plain
            };
            let word = text(index, end);
            emit(&mut out, tok, word);
            command_position = false;
            index = end;
            continue;
        }

        if lang.family == Family::Shell && "\n;|&(".contains(ch) {
            command_position = true;
        }
        let tok = if "{}[]()<>;:,.=+-*/%&|!?^~".contains(ch) {
            Tok::Punct
        } else {
            Tok::Plain
        };
        emit(&mut out, tok, ch.to_string());
        index += 1;
    }
    out
}

/// Tags, attribute names and quoted values. Text between tags is plain.
fn lex_markup(chars: &[char]) -> Vec<(Tok, String)> {
    let mut out: Vec<(Tok, String)> = Vec::new();
    let mut index = 0;
    let text = |from: usize, to: usize| chars[from..to].iter().collect::<String>();
    let emit = |out: &mut Vec<(Tok, String)>, tok: Tok, piece: String| match out.last_mut() {
        Some((last, have)) if *last == tok => have.push_str(&piece),
        _ => out.push((tok, piece)),
    };
    while index < chars.len() {
        if starts(chars, index, "<!--") {
            let end = until(chars, index + 4, "-->");
            emit(&mut out, Tok::Comment, text(index, end));
            index = end;
            continue;
        }
        if chars[index] == '<' {
            let mut end = index + 1;
            if chars.get(end) == Some(&'/') {
                end += 1;
            }
            emit(&mut out, Tok::Punct, text(index, end));
            index = end;
            while end < chars.len()
                && (is_ident(chars[end]) || chars[end] == '-' || chars[end] == ':')
            {
                end += 1;
            }
            emit(&mut out, Tok::Keyword, text(index, end));
            index = end;
            while index < chars.len() && chars[index] != '>' {
                let ch = chars[index];
                if ch == '"' || ch == '\'' {
                    let end = string_end(chars, index, ch, true);
                    emit(&mut out, Tok::Str, text(index, end));
                    index = end;
                } else if is_ident_start(ch) {
                    let mut end = index;
                    while end < chars.len()
                        && (is_ident(chars[end]) || chars[end] == '-' || chars[end] == ':')
                    {
                        end += 1;
                    }
                    emit(&mut out, Tok::Attr, text(index, end));
                    index = end;
                } else {
                    emit(&mut out, Tok::Punct, ch.to_string());
                    index += 1;
                }
            }
            if index < chars.len() {
                emit(&mut out, Tok::Punct, ">".to_string());
                index += 1;
            }
            continue;
        }
        emit(&mut out, Tok::Plain, chars[index].to_string());
        index += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(lines: &[Vec<(Tok, String)>], needle: &str) -> Option<Tok> {
        lines
            .iter()
            .flatten()
            .find(|(_, text)| text.trim() == needle)
            .map(|(tok, _)| *tok)
    }

    #[test]
    fn rust_keywords_strings_macros_and_types() {
        let lines = highlight(
            "rust",
            "// note\nfn main() {\n    let name: String = \"hi\";\n    println!(\"{}\", 42);\n}",
        );
        assert_eq!(lines.len(), 5);
        assert_eq!(kind_of(&lines, "// note"), Some(Tok::Comment));
        assert_eq!(kind_of(&lines, "fn"), Some(Tok::Keyword));
        assert_eq!(kind_of(&lines, "main"), Some(Tok::Function));
        assert_eq!(kind_of(&lines, "String"), Some(Tok::Type));
        assert_eq!(kind_of(&lines, "\"hi\""), Some(Tok::Str));
        assert_eq!(kind_of(&lines, "println!"), Some(Tok::Macro));
        assert_eq!(kind_of(&lines, "42"), Some(Tok::Number));
    }

    #[test]
    fn a_rust_lifetime_is_not_an_open_string() {
        let lines = highlight("rust", "fn f<'a>(x: &'a str) -> char { 'z' }");
        assert_eq!(kind_of(&lines, "'a"), Some(Tok::Attr));
        assert_eq!(kind_of(&lines, "'z'"), Some(Tok::Str));
        assert_eq!(kind_of(&lines, "str"), Some(Tok::Type));
    }

    #[test]
    fn a_python_docstring_spans_lines() {
        let lines = highlight(
            "py",
            "def f():\n    \"\"\"one\n    two\"\"\"\n    return None",
        );
        assert_eq!(kind_of(&lines, "def"), Some(Tok::Keyword));
        assert!(lines[2].iter().all(|(tok, _)| *tok == Tok::Str));
        assert_eq!(kind_of(&lines, "None"), Some(Tok::Constant));
    }

    #[test]
    fn shell_commands_flags_and_variables() {
        let lines = highlight("sh", "cargo test --release # run\necho $HOME | grep x");
        assert_eq!(kind_of(&lines, "cargo"), Some(Tok::Function));
        assert_eq!(kind_of(&lines, "--release"), Some(Tok::Attr));
        assert_eq!(kind_of(&lines, "# run"), Some(Tok::Comment));
        assert_eq!(kind_of(&lines, "$HOME"), Some(Tok::Constant));
        assert_eq!(kind_of(&lines, "grep"), Some(Tok::Function));
    }

    #[test]
    fn json_keys_differ_from_values() {
        let lines = highlight("json", "{\"name\": \"token\", \"ok\": true}");
        assert_eq!(kind_of(&lines, "\"name\""), Some(Tok::Attr));
        assert_eq!(kind_of(&lines, "\"token\""), Some(Tok::Str));
        assert_eq!(kind_of(&lines, "true"), Some(Tok::Constant));
    }

    #[test]
    fn toml_sections_and_keys() {
        let lines = highlight("toml", "[package]\nname = \"token\"");
        assert_eq!(kind_of(&lines, "[package]"), Some(Tok::Type));
        assert_eq!(kind_of(&lines, "name"), Some(Tok::Attr));
    }

    #[test]
    fn html_tags_and_attributes() {
        let lines = highlight("html", "<a href=\"x\">hi</a>");
        assert_eq!(kind_of(&lines, "a"), Some(Tok::Keyword));
        assert_eq!(kind_of(&lines, "href"), Some(Tok::Attr));
        assert_eq!(kind_of(&lines, "\"x\""), Some(Tok::Str));
    }

    #[test]
    fn nothing_is_dropped() {
        let body = "weird ¿ input\twith 'unclosed \"strings";
        let joined: String = highlight("", body)
            .iter()
            .map(|line| {
                line.iter()
                    .map(|(_, text)| text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(joined, body);
    }
}
