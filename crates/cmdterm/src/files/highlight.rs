//! Syntax highlighting for the file reader.
//!
//! A language is a table: comment and string delimiters plus sorted word
//! lists. [`highlight`] classifies one line, starting from the [`State`] the
//! line above ended in, and reports contiguous byte ranges with a [`Token`]
//! each. It never allocates. The reader runs it for every row it patches, and
//! the loader runs it once over the whole file, on the loader's thread, to
//! record the state each line starts in.
//!
//! Ranges end on ASCII bytes or at the end of the line, so they always fall on
//! char boundaries.

use std::cmp::Ordering;
use std::path::Path;

/// What a range of a line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    Plain,
    Keyword,
    Type,
    /// Function names at calls and definitions, macros, shell builtins.
    Function,
    String,
    Number,
    /// Literals such as `true` and `None`, `ALL_CAPS` names, variables.
    Constant,
    Comment,
    /// Attributes, decorators, preprocessor lines, lifetimes.
    Attribute,
    /// Keys in configuration files, attribute names in markup.
    Property,
    Heading,
}

/// Where the lexer is at the start of a line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Normal,
    /// Inside a block comment, nested `depth` deep.
    Comment(u8),
    /// Inside a string that spans lines: the language's quote number
    /// `quote`, or a Rust raw string with `hashes` when `quote` is [`RAW`].
    String { quote: u8, hashes: u8 },
    /// Markdown: inside a fenced code block.
    Fence,
    /// HTML and XML: inside a tag, between attributes.
    Tag,
}

/// The `quote` of a Rust raw string.
const RAW: u8 = u8::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Code,
    Markdown,
    Diff,
    Markup,
}

/// Which words count as keys of a configuration file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keys {
    None,
    /// The first word of a line followed by `=`.
    Assign,
    /// The first word of a line, or any string, followed by `:`.
    Colon,
}

struct Quote {
    open: &'static str,
    close: &'static str,
    /// Backslash escapes the next byte.
    escape: bool,
    multiline: bool,
}

const DQ: Quote = Quote {
    open: "\"",
    close: "\"",
    escape: true,
    multiline: false,
};
const DQ_LINES: Quote = Quote {
    multiline: true,
    ..DQ
};
const SQ: Quote = Quote {
    open: "'",
    close: "'",
    escape: true,
    multiline: false,
};
const SQ_RAW: Quote = Quote {
    escape: false,
    ..SQ
};
const SQ_LINES_RAW: Quote = Quote {
    multiline: true,
    ..SQ_RAW
};
const BACKTICK: Quote = Quote {
    open: "`",
    close: "`",
    escape: true,
    multiline: true,
};
const BACKTICK_RAW: Quote = Quote {
    escape: false,
    ..BACKTICK
};
const TRIPLE_DQ: Quote = Quote {
    open: "\"\"\"",
    close: "\"\"\"",
    escape: true,
    multiline: true,
};
const TRIPLE_SQ: Quote = Quote {
    open: "'''",
    close: "'''",
    escape: true,
    multiline: true,
};

/// One language's lexer table.
pub struct Language {
    pub name: &'static str,
    kind: Kind,
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    nested_comments: bool,
    /// Tried in order, so a longer opener comes before its prefix.
    quotes: &'static [Quote],
    /// The word lists are sorted bytewise, lowercase when `ignore_case`.
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    constants: &'static [&'static str],
    builtins: &'static [&'static str],
    ignore_case: bool,
    /// `Capitalized` words are types and `ALL_CAPS` words constants.
    upper_types: bool,
    /// `name!` is a macro call.
    macros: bool,
    /// Rust's raw strings, char literals, lifetimes and `#[attributes]`.
    rust_literals: bool,
    /// The byte that starts a decorator or directive: `@name`, `#include`.
    decorator: Option<u8>,
    /// `$name` and `${name}` are variables.
    variables: bool,
    keys: Keys,
    /// A line starting with `[` is a section header.
    sections: bool,
    /// Words may contain `-`, as in `apt-get` or `max-width`.
    dash_words: bool,
    /// A line comment starts only at the beginning of a word.
    word_comments: bool,
    /// Short words directly before a quote are string prefixes: `b"`, `f'`.
    string_prefixes: bool,
}

const BASE: Language = Language {
    name: "",
    kind: Kind::Code,
    line_comments: &[],
    block_comment: None,
    nested_comments: false,
    quotes: &[DQ, SQ],
    keywords: &[],
    types: &[],
    constants: &[],
    builtins: &[],
    ignore_case: false,
    upper_types: false,
    macros: false,
    rust_literals: false,
    decorator: None,
    variables: false,
    keys: Keys::None,
    sections: false,
    dash_words: false,
    word_comments: false,
    string_prefixes: false,
};

const C_COMMENTS: Language = Language {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    ..BASE
};

pub const RUST: Language = Language {
    name: "Rust",
    nested_comments: true,
    quotes: &[DQ_LINES],
    keywords: &[
        "as", "async", "await", "box", "break", "const", "continue", "crate", "do", "dyn", "else",
        "enum", "extern", "fn", "for", "if", "impl", "in", "let", "loop", "macro", "match", "mod",
        "move", "mut", "pub", "ref", "return", "self", "static", "struct", "super", "trait", "try",
        "type", "union", "unsafe", "use", "where", "while", "yield",
    ],
    types: &[
        "Self", "bool", "char", "f32", "f64", "i128", "i16", "i32", "i64", "i8", "isize", "str",
        "u128", "u16", "u32", "u64", "u8", "usize",
    ],
    constants: &["Err", "None", "Ok", "Some", "false", "true"],
    upper_types: true,
    macros: true,
    rust_literals: true,
    string_prefixes: true,
    ..C_COMMENTS
};

const C_KEYWORDS: &[&str] = &[
    "_Alignas",
    "_Alignof",
    "_Atomic",
    "_Generic",
    "_Noreturn",
    "_Static_assert",
    "_Thread_local",
    "auto",
    "break",
    "case",
    "const",
    "continue",
    "default",
    "do",
    "else",
    "enum",
    "extern",
    "for",
    "goto",
    "if",
    "inline",
    "register",
    "restrict",
    "return",
    "sizeof",
    "static",
    "struct",
    "switch",
    "typedef",
    "union",
    "volatile",
    "while",
];
const C_TYPES: &[&str] = &[
    "FILE",
    "bool",
    "char",
    "double",
    "float",
    "int",
    "int16_t",
    "int32_t",
    "int64_t",
    "int8_t",
    "intptr_t",
    "long",
    "ptrdiff_t",
    "short",
    "signed",
    "size_t",
    "ssize_t",
    "uint16_t",
    "uint32_t",
    "uint64_t",
    "uint8_t",
    "uintptr_t",
    "unsigned",
    "void",
];

pub const C: Language = Language {
    name: "C",
    keywords: C_KEYWORDS,
    types: C_TYPES,
    constants: &["NULL", "false", "true"],
    upper_types: true,
    decorator: Some(b'#'),
    string_prefixes: true,
    ..C_COMMENTS
};

pub const CPP: Language = Language {
    name: "C++",
    keywords: &[
        "alignas",
        "alignof",
        "and",
        "asm",
        "auto",
        "break",
        "case",
        "catch",
        "class",
        "co_await",
        "co_return",
        "co_yield",
        "concept",
        "const",
        "const_cast",
        "consteval",
        "constexpr",
        "constinit",
        "continue",
        "decltype",
        "default",
        "delete",
        "do",
        "dynamic_cast",
        "else",
        "enum",
        "explicit",
        "export",
        "extern",
        "final",
        "for",
        "friend",
        "goto",
        "if",
        "inline",
        "mutable",
        "namespace",
        "new",
        "noexcept",
        "not",
        "operator",
        "or",
        "override",
        "private",
        "protected",
        "public",
        "register",
        "reinterpret_cast",
        "requires",
        "return",
        "sizeof",
        "static",
        "static_assert",
        "static_cast",
        "struct",
        "switch",
        "template",
        "this",
        "thread_local",
        "throw",
        "try",
        "typedef",
        "typeid",
        "typename",
        "union",
        "using",
        "virtual",
        "volatile",
        "while",
    ],
    types: &[
        "bool", "char", "char16_t", "char32_t", "char8_t", "double", "float", "int", "int16_t",
        "int32_t", "int64_t", "int8_t", "long", "short", "signed", "size_t", "uint16_t",
        "uint32_t", "uint64_t", "uint8_t", "unsigned", "void", "wchar_t",
    ],
    constants: &["NULL", "false", "nullptr", "true"],
    ..C
};

pub const GO: Language = Language {
    name: "Go",
    quotes: &[DQ, SQ, BACKTICK_RAW],
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
        "any",
        "bool",
        "byte",
        "complex128",
        "complex64",
        "error",
        "float32",
        "float64",
        "int",
        "int16",
        "int32",
        "int64",
        "int8",
        "rune",
        "string",
        "uint",
        "uint16",
        "uint32",
        "uint64",
        "uint8",
        "uintptr",
    ],
    constants: &["false", "iota", "nil", "true"],
    builtins: &[
        "append", "cap", "close", "copy", "delete", "len", "make", "new", "panic", "print",
        "println", "recover",
    ],
    ..C_COMMENTS
};

pub const PYTHON: Language = Language {
    name: "Python",
    line_comments: &["#"],
    quotes: &[TRIPLE_DQ, TRIPLE_SQ, DQ, SQ],
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "case", "class", "continue", "def",
        "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in",
        "is", "lambda", "match", "nonlocal", "not", "or", "pass", "raise", "return", "try",
        "while", "with", "yield",
    ],
    types: &[
        "bool",
        "bytearray",
        "bytes",
        "complex",
        "dict",
        "float",
        "frozenset",
        "int",
        "list",
        "object",
        "set",
        "str",
        "tuple",
        "type",
    ],
    constants: &["False", "None", "NotImplemented", "True", "cls", "self"],
    builtins: &[
        "abs",
        "all",
        "any",
        "enumerate",
        "filter",
        "getattr",
        "hasattr",
        "isinstance",
        "iter",
        "len",
        "map",
        "max",
        "min",
        "next",
        "open",
        "print",
        "range",
        "repr",
        "setattr",
        "sorted",
        "sum",
        "super",
        "zip",
    ],
    upper_types: true,
    decorator: Some(b'@'),
    string_prefixes: true,
    ..BASE
};

const JS_KEYWORDS: &[&str] = &[
    "as",
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
    "get",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "of",
    "return",
    "set",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

pub const JAVASCRIPT: Language = Language {
    name: "JavaScript",
    quotes: &[DQ, SQ, BACKTICK],
    keywords: JS_KEYWORDS,
    constants: &["Infinity", "NaN", "false", "null", "true", "undefined"],
    upper_types: true,
    decorator: Some(b'@'),
    ..C_COMMENTS
};

pub const TYPESCRIPT: Language = Language {
    name: "TypeScript",
    keywords: &[
        "abstract",
        "as",
        "asserts",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "debugger",
        "declare",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "export",
        "extends",
        "finally",
        "for",
        "from",
        "function",
        "get",
        "if",
        "implements",
        "import",
        "in",
        "infer",
        "instanceof",
        "interface",
        "is",
        "keyof",
        "let",
        "module",
        "namespace",
        "new",
        "of",
        "override",
        "private",
        "protected",
        "public",
        "readonly",
        "return",
        "satisfies",
        "set",
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
    ],
    types: &[
        "any", "bigint", "boolean", "never", "number", "object", "string", "symbol", "unknown",
    ],
    ..JAVASCRIPT
};

pub const JAVA: Language = Language {
    name: "Java",
    keywords: &[
        "abstract",
        "assert",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "default",
        "do",
        "else",
        "enum",
        "extends",
        "final",
        "finally",
        "for",
        "goto",
        "if",
        "implements",
        "import",
        "instanceof",
        "interface",
        "native",
        "new",
        "package",
        "permits",
        "private",
        "protected",
        "public",
        "record",
        "return",
        "sealed",
        "static",
        "strictfp",
        "super",
        "switch",
        "synchronized",
        "this",
        "throw",
        "throws",
        "transient",
        "try",
        "var",
        "volatile",
        "while",
        "yield",
    ],
    types: &[
        "boolean", "byte", "char", "double", "float", "int", "long", "short", "void",
    ],
    constants: &["false", "null", "true"],
    upper_types: true,
    decorator: Some(b'@'),
    ..C_COMMENTS
};

pub const KOTLIN: Language = Language {
    name: "Kotlin",
    quotes: &[TRIPLE_DQ, DQ, SQ],
    keywords: &[
        "abstract",
        "actual",
        "annotation",
        "as",
        "break",
        "by",
        "catch",
        "class",
        "companion",
        "const",
        "constructor",
        "continue",
        "crossinline",
        "data",
        "do",
        "else",
        "enum",
        "expect",
        "external",
        "final",
        "finally",
        "for",
        "fun",
        "get",
        "if",
        "import",
        "in",
        "infix",
        "init",
        "inline",
        "inner",
        "interface",
        "internal",
        "is",
        "lateinit",
        "noinline",
        "object",
        "open",
        "operator",
        "out",
        "override",
        "package",
        "private",
        "protected",
        "public",
        "reified",
        "return",
        "sealed",
        "set",
        "super",
        "suspend",
        "tailrec",
        "this",
        "throw",
        "try",
        "typealias",
        "val",
        "var",
        "vararg",
        "when",
        "where",
        "while",
    ],
    constants: &["false", "null", "true"],
    upper_types: true,
    decorator: Some(b'@'),
    ..C_COMMENTS
};

pub const CSHARP: Language = Language {
    name: "C#",
    keywords: &[
        "abstract",
        "as",
        "async",
        "await",
        "base",
        "break",
        "case",
        "catch",
        "checked",
        "class",
        "const",
        "continue",
        "default",
        "delegate",
        "do",
        "else",
        "enum",
        "event",
        "explicit",
        "extern",
        "finally",
        "fixed",
        "for",
        "foreach",
        "get",
        "goto",
        "if",
        "implicit",
        "in",
        "init",
        "interface",
        "internal",
        "is",
        "lock",
        "namespace",
        "new",
        "operator",
        "out",
        "override",
        "params",
        "private",
        "protected",
        "public",
        "readonly",
        "record",
        "ref",
        "return",
        "sealed",
        "set",
        "sizeof",
        "stackalloc",
        "static",
        "struct",
        "switch",
        "this",
        "throw",
        "try",
        "typeof",
        "unchecked",
        "unsafe",
        "using",
        "value",
        "var",
        "virtual",
        "volatile",
        "where",
        "while",
        "yield",
    ],
    types: &[
        "bool", "byte", "char", "decimal", "double", "dynamic", "float", "int", "long", "object",
        "sbyte", "short", "string", "uint", "ulong", "ushort", "void",
    ],
    constants: &["false", "null", "true"],
    upper_types: true,
    ..C_COMMENTS
};

pub const ZIG: Language = Language {
    name: "Zig",
    line_comments: &["//"],
    keywords: &[
        "addrspace",
        "align",
        "allowzero",
        "and",
        "anyframe",
        "anytype",
        "asm",
        "async",
        "await",
        "break",
        "callconv",
        "catch",
        "comptime",
        "const",
        "continue",
        "defer",
        "else",
        "enum",
        "errdefer",
        "error",
        "export",
        "extern",
        "fn",
        "for",
        "if",
        "inline",
        "linksection",
        "noalias",
        "noinline",
        "nosuspend",
        "opaque",
        "or",
        "orelse",
        "packed",
        "pub",
        "resume",
        "return",
        "struct",
        "suspend",
        "switch",
        "test",
        "threadlocal",
        "try",
        "union",
        "unreachable",
        "usingnamespace",
        "var",
        "volatile",
        "while",
    ],
    types: &[
        "anyerror",
        "bool",
        "c_int",
        "comptime_float",
        "comptime_int",
        "f128",
        "f16",
        "f32",
        "f64",
        "i128",
        "i16",
        "i32",
        "i64",
        "i8",
        "isize",
        "noreturn",
        "type",
        "u128",
        "u16",
        "u32",
        "u64",
        "u8",
        "usize",
        "void",
    ],
    constants: &["false", "null", "true", "undefined"],
    upper_types: true,
    decorator: Some(b'@'),
    ..BASE
};

pub const RUBY: Language = Language {
    name: "Ruby",
    line_comments: &["#"],
    keywords: &[
        "BEGIN", "END", "alias", "and", "begin", "break", "case", "class", "def", "do", "else",
        "elsif", "end", "ensure", "for", "if", "in", "module", "next", "not", "or", "redo",
        "rescue", "retry", "return", "super", "then", "undef", "unless", "until", "when", "while",
        "yield",
    ],
    constants: &["false", "nil", "self", "true"],
    builtins: &[
        "attr_accessor",
        "attr_reader",
        "puts",
        "require",
        "require_relative",
    ],
    upper_types: true,
    decorator: Some(b'@'),
    ..BASE
};

pub const SHELL: Language = Language {
    name: "Shell",
    line_comments: &["#"],
    quotes: &[DQ_LINES, SQ_LINES_RAW],
    keywords: &[
        "case", "coproc", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if",
        "in", "select", "then", "time", "until", "while",
    ],
    builtins: &[
        "alias", "cd", "command", "declare", "echo", "eval", "exec", "exit", "export", "local",
        "printf", "read", "readonly", "return", "set", "shift", "source", "test", "trap", "unset",
    ],
    constants: &["false", "true"],
    variables: true,
    dash_words: true,
    word_comments: true,
    ..BASE
};

pub const LUA: Language = Language {
    name: "Lua",
    line_comments: &["--"],
    block_comment: Some(("--[[", "]]")),
    quotes: &[
        DQ,
        SQ,
        Quote {
            open: "[[",
            close: "]]",
            escape: false,
            multiline: true,
        },
    ],
    keywords: &[
        "and", "break", "do", "else", "elseif", "end", "for", "function", "goto", "if", "in",
        "local", "not", "or", "repeat", "return", "then", "until", "while",
    ],
    constants: &["false", "nil", "self", "true"],
    builtins: &[
        "assert",
        "error",
        "getmetatable",
        "ipairs",
        "next",
        "pairs",
        "pcall",
        "print",
        "rawget",
        "rawset",
        "require",
        "select",
        "setmetatable",
        "tonumber",
        "tostring",
        "type",
        "unpack",
        "xpcall",
    ],
    ..BASE
};

pub const TOML: Language = Language {
    name: "TOML",
    line_comments: &["#"],
    quotes: &[
        TRIPLE_DQ,
        Quote {
            escape: false,
            ..TRIPLE_SQ
        },
        DQ,
        SQ_RAW,
    ],
    constants: &["false", "inf", "nan", "true"],
    keys: Keys::Assign,
    sections: true,
    dash_words: true,
    ..BASE
};

pub const INI: Language = Language {
    name: "INI",
    line_comments: &["#", ";"],
    quotes: &[DQ],
    constants: &["false", "no", "off", "on", "true", "yes"],
    keys: Keys::Assign,
    sections: true,
    dash_words: true,
    ..BASE
};

pub const YAML: Language = Language {
    name: "YAML",
    line_comments: &["#"],
    quotes: &[DQ, SQ_RAW],
    constants: &["false", "no", "null", "off", "on", "true", "yes"],
    keys: Keys::Colon,
    dash_words: true,
    word_comments: true,
    ..BASE
};

pub const JSON: Language = Language {
    name: "JSON",
    quotes: &[DQ],
    constants: &["false", "null", "true"],
    keys: Keys::Colon,
    ..C_COMMENTS
};

pub const CSS: Language = Language {
    name: "CSS",
    line_comments: &[],
    constants: &["auto", "inherit", "initial", "none", "unset"],
    decorator: Some(b'@'),
    keys: Keys::Colon,
    dash_words: true,
    ..C_COMMENTS
};

pub const SQL: Language = Language {
    name: "SQL",
    line_comments: &["--"],
    block_comment: Some(("/*", "*/")),
    keywords: &[
        "add",
        "all",
        "alter",
        "and",
        "as",
        "asc",
        "begin",
        "between",
        "by",
        "cascade",
        "case",
        "check",
        "column",
        "commit",
        "constraint",
        "create",
        "cross",
        "declare",
        "default",
        "delete",
        "desc",
        "distinct",
        "drop",
        "else",
        "end",
        "exists",
        "foreign",
        "from",
        "full",
        "function",
        "grant",
        "group",
        "having",
        "if",
        "in",
        "index",
        "inner",
        "insert",
        "into",
        "is",
        "join",
        "key",
        "left",
        "like",
        "limit",
        "not",
        "offset",
        "on",
        "or",
        "order",
        "outer",
        "primary",
        "recursive",
        "references",
        "return",
        "returns",
        "revoke",
        "right",
        "rollback",
        "select",
        "set",
        "table",
        "then",
        "transaction",
        "trigger",
        "union",
        "unique",
        "update",
        "values",
        "view",
        "when",
        "where",
        "with",
    ],
    types: &[
        "bigint",
        "blob",
        "bool",
        "boolean",
        "char",
        "date",
        "decimal",
        "double",
        "float",
        "int",
        "integer",
        "json",
        "jsonb",
        "numeric",
        "real",
        "serial",
        "smallint",
        "text",
        "time",
        "timestamp",
        "uuid",
        "varchar",
    ],
    constants: &["false", "null", "true"],
    ignore_case: true,
    ..BASE
};

pub const NIX: Language = Language {
    name: "Nix",
    line_comments: &["#"],
    block_comment: Some(("/*", "*/")),
    quotes: &[
        DQ_LINES,
        Quote {
            open: "''",
            close: "''",
            escape: false,
            multiline: true,
        },
    ],
    keywords: &[
        "assert", "else", "if", "in", "inherit", "let", "or", "rec", "then", "with",
    ],
    constants: &["false", "null", "true"],
    builtins: &["builtins", "derivation", "fetchTarball", "import"],
    keys: Keys::Assign,
    dash_words: true,
    ..BASE
};

pub const MAKE: Language = Language {
    name: "Makefile",
    line_comments: &["#"],
    keywords: &[
        "define", "else", "endef", "endif", "export", "ifdef", "ifeq", "ifndef", "ifneq",
        "include", "override", "unexport",
    ],
    variables: true,
    keys: Keys::Colon,
    dash_words: true,
    ..BASE
};

pub const DOCKERFILE: Language = Language {
    name: "Dockerfile",
    line_comments: &["#"],
    keywords: &[
        "add",
        "arg",
        "as",
        "cmd",
        "copy",
        "entrypoint",
        "env",
        "expose",
        "from",
        "healthcheck",
        "label",
        "maintainer",
        "onbuild",
        "run",
        "shell",
        "stopsignal",
        "user",
        "volume",
        "workdir",
    ],
    ignore_case: true,
    variables: true,
    dash_words: true,
    word_comments: true,
    ..BASE
};

pub const MARKDOWN: Language = Language {
    name: "Markdown",
    kind: Kind::Markdown,
    ..BASE
};

pub const DIFF: Language = Language {
    name: "Diff",
    kind: Kind::Diff,
    ..BASE
};

pub const HTML: Language = Language {
    name: "HTML",
    kind: Kind::Markup,
    ..BASE
};

pub const XML: Language = Language {
    name: "XML",
    ..HTML
};

/// Every language.
#[cfg(test)]
const LANGUAGES: &[&Language] = &[
    &RUST,
    &C,
    &CPP,
    &GO,
    &PYTHON,
    &JAVASCRIPT,
    &TYPESCRIPT,
    &JAVA,
    &KOTLIN,
    &CSHARP,
    &ZIG,
    &RUBY,
    &SHELL,
    &LUA,
    &TOML,
    &INI,
    &YAML,
    &JSON,
    &CSS,
    &SQL,
    &NIX,
    &MAKE,
    &DOCKERFILE,
    &MARKDOWN,
    &DIFF,
    &HTML,
    &XML,
];

/// File extensions, compared without case.
const EXTENSIONS: &[(&str, &Language)] = &[
    ("rs", &RUST),
    ("c", &C),
    ("h", &C),
    ("cc", &CPP),
    ("cpp", &CPP),
    ("cxx", &CPP),
    ("hh", &CPP),
    ("hpp", &CPP),
    ("hxx", &CPP),
    ("go", &GO),
    ("py", &PYTHON),
    ("pyi", &PYTHON),
    ("js", &JAVASCRIPT),
    ("mjs", &JAVASCRIPT),
    ("cjs", &JAVASCRIPT),
    ("jsx", &JAVASCRIPT),
    ("ts", &TYPESCRIPT),
    ("tsx", &TYPESCRIPT),
    ("mts", &TYPESCRIPT),
    ("cts", &TYPESCRIPT),
    ("java", &JAVA),
    ("kt", &KOTLIN),
    ("kts", &KOTLIN),
    ("cs", &CSHARP),
    ("zig", &ZIG),
    ("rb", &RUBY),
    ("sh", &SHELL),
    ("bash", &SHELL),
    ("zsh", &SHELL),
    ("ksh", &SHELL),
    ("fish", &SHELL),
    ("lua", &LUA),
    ("toml", &TOML),
    ("ini", &INI),
    ("cfg", &INI),
    ("conf", &INI),
    ("desktop", &INI),
    ("service", &INI),
    ("yaml", &YAML),
    ("yml", &YAML),
    ("json", &JSON),
    ("jsonc", &JSON),
    ("json5", &JSON),
    ("css", &CSS),
    ("scss", &CSS),
    ("less", &CSS),
    ("sql", &SQL),
    ("nix", &NIX),
    ("mk", &MAKE),
    ("md", &MARKDOWN),
    ("markdown", &MARKDOWN),
    ("diff", &DIFF),
    ("patch", &DIFF),
    ("html", &HTML),
    ("htm", &HTML),
    ("xhtml", &HTML),
    ("vue", &HTML),
    ("svelte", &HTML),
    ("xml", &XML),
    ("svg", &XML),
    ("plist", &XML),
    ("xsd", &XML),
    ("xsl", &XML),
];

/// Whole file names that say more than their extension.
const FILE_NAMES: &[(&str, &Language)] = &[
    ("Makefile", &MAKE),
    ("makefile", &MAKE),
    ("GNUmakefile", &MAKE),
    ("Justfile", &MAKE),
    ("justfile", &MAKE),
    ("Dockerfile", &DOCKERFILE),
    ("Containerfile", &DOCKERFILE),
    ("PKGBUILD", &SHELL),
    ("APKBUILD", &SHELL),
    (".bashrc", &SHELL),
    (".bash_profile", &SHELL),
    (".zshrc", &SHELL),
    (".profile", &SHELL),
    (".envrc", &SHELL),
    ("Cargo.lock", &TOML),
    (".gitconfig", &INI),
    (".editorconfig", &INI),
    ("flake.lock", &JSON),
];

/// The language a Markdown code fence names: an extension such as `rs` or a
/// language name such as `rust`, case ignored.
pub fn by_name(name: &str) -> Option<&'static Language> {
    EXTENSIONS
        .iter()
        .find(|(ext, language)| {
            ext.eq_ignore_ascii_case(name) || language.name.eq_ignore_ascii_case(name)
        })
        .map(|(_, language)| *language)
}

/// The language of the file at `path`, from its name, its extension, or a
/// `#!` interpreter line.
pub fn detect(path: &Path, first_line: &[u8]) -> Option<&'static Language> {
    let name = path.file_name()?.to_str()?;
    if let Some((_, language)) = FILE_NAMES.iter().find(|(n, _)| *n == name) {
        return Some(language);
    }
    if name.starts_with("Dockerfile.") || name.ends_with(".Dockerfile") {
        return Some(&DOCKERFILE);
    }
    if let Some((_, ext)) = name.rsplit_once('.')
        && let Some((_, language)) = EXTENSIONS.iter().find(|(e, _)| e.eq_ignore_ascii_case(ext))
    {
        return Some(language);
    }
    let interpreter = first_line.strip_prefix(b"#!")?;
    let program = interpreter
        .split(|b| b.is_ascii_whitespace() || *b == b'/')
        .rfind(|word| !word.is_empty() && *word != b"env" && !word.starts_with(b"-"))?;
    let language: &Language = match program {
        b"sh" | b"bash" | b"zsh" | b"dash" | b"ksh" | b"fish" => &SHELL,
        b"python" | b"python3" | b"python2" => &PYTHON,
        b"node" | b"deno" | b"bun" => &JAVASCRIPT,
        b"lua" | b"luajit" => &LUA,
        b"ruby" => &RUBY,
        _ => return None,
    };
    Some(language)
}

/// Classifies `line` from `state` on, calling `emit(start, end, token)` for
/// contiguous byte ranges that cover the whole line. Leaves in `state` where
/// the next line starts.
pub fn highlight(
    language: &Language,
    state: &mut State,
    line: &str,
    mut emit: impl FnMut(usize, usize, Token),
) {
    let bytes = line.as_bytes();
    let mut out = Out {
        emit: &mut emit,
        pos: 0,
        len: bytes.len(),
    };
    match language.kind {
        Kind::Code => code(language, state, bytes, &mut out),
        Kind::Markdown => markdown(state, bytes, &mut out),
        Kind::Diff => diff(bytes, &mut out),
        Kind::Markup => markup(state, bytes, &mut out),
    }
    out.finish();
}

/// The state the line after `line` starts in.
pub fn advance(language: &Language, mut state: State, line: &str) -> State {
    highlight(language, &mut state, line, |_, _, _| {});
    state
}

/// Fills gaps between reported ranges with [`Token::Plain`].
struct Out<'a, F> {
    emit: &'a mut F,
    pos: usize,
    len: usize,
}

impl<F: FnMut(usize, usize, Token)> Out<'_, F> {
    fn span(&mut self, start: usize, end: usize, token: Token) {
        let end = end.min(self.len);
        let start = start.max(self.pos);
        if start > self.pos {
            (self.emit)(self.pos, start, Token::Plain);
        }
        if end > start {
            (self.emit)(start, end, token);
            self.pos = end;
        } else {
            self.pos = self.pos.max(start);
        }
    }

    fn finish(&mut self) {
        if self.pos < self.len {
            (self.emit)(self.pos, self.len, Token::Plain);
            self.pos = self.len;
        }
    }
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

fn is_word_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

/// The first byte at or after `from` that is not a space or tab.
fn next_byte(bytes: &[u8], from: usize) -> Option<u8> {
    bytes
        .get(from..)?
        .iter()
        .copied()
        .find(|b| *b != b' ' && *b != b'\t')
}

/// Whether only spaces, tabs and, with `dash`, one list `-` come before
/// `at` on its line.
fn starts_line(bytes: &[u8], at: usize, dash: bool) -> bool {
    let before = bytes[..at].trim_ascii();
    before.is_empty() || (dash && before == b"-")
}

fn word_end(language: &Language, bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() {
        let byte = bytes[i];
        let dash = language.dash_words
            && byte == b'-'
            && bytes.get(i + 1).is_some_and(|next| is_word(*next));
        if !is_word(byte) && !dash {
            break;
        }
        i += 1;
    }
    i
}

fn number_end(bytes: &[u8], from: usize) -> usize {
    let mut i = from + 1;
    while i < bytes.len() {
        let byte = bytes[i];
        let fraction = byte == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit);
        let exponent = matches!(byte, b'+' | b'-') && matches!(bytes[i - 1], b'e' | b'E');
        if !(byte.is_ascii_alphanumeric() || byte == b'_' || fraction || exponent) {
            break;
        }
        i += 1;
    }
    i
}

/// Whether the sorted `list` holds `word`. With `ignore_case`, `list` is
/// lowercase and `word` is compared as if it were.
fn holds(list: &[&str], word: &[u8], ignore_case: bool) -> bool {
    if !ignore_case {
        return list
            .binary_search_by(|item| item.as_bytes().cmp(word))
            .is_ok();
    }
    list.binary_search_by(|item| {
        let item = item.as_bytes();
        for (a, b) in item.iter().zip(word) {
            match a.cmp(&b.to_ascii_lowercase()) {
                Ordering::Equal => {}
                other => return other,
            }
        }
        item.len().cmp(&word.len())
    })
    .is_ok()
}

fn quote_at(language: &Language, rest: &[u8]) -> Option<usize> {
    language
        .quotes
        .iter()
        .position(|quote| rest.starts_with(quote.open.as_bytes()))
}

fn code<F: FnMut(usize, usize, Token)>(
    language: &Language,
    state: &mut State,
    bytes: &[u8],
    out: &mut Out<'_, F>,
) {
    let mut i = 0;
    match *state {
        State::Comment(depth) => {
            i = comment_end(language, bytes, 0, depth, state);
            out.span(0, i, Token::Comment);
        }
        State::String { quote, hashes } => {
            i = string_end(language, bytes, 0, quote, hashes, state);
            out.span(0, i, Token::String);
        }
        State::Normal => {}
        State::Fence | State::Tag => *state = State::Normal,
    }
    if i == 0 && language.sections {
        let text = bytes.trim_ascii_start();
        if text.starts_with(b"[") {
            out.span(bytes.len() - text.len(), bytes.len(), Token::Type);
            return;
        }
    }
    while i < bytes.len() {
        let byte = bytes[i];
        let rest = &bytes[i..];
        if byte == b' ' || byte == b'\t' {
            i += 1;
            continue;
        }
        if let Some((open, _)) = language.block_comment
            && rest.starts_with(open.as_bytes())
        {
            let start = i;
            i = comment_end(language, bytes, i + open.len(), 1, state);
            out.span(start, i, Token::Comment);
            continue;
        }
        let comment = language
            .line_comments
            .iter()
            .any(|marker| rest.starts_with(marker.as_bytes()));
        if comment
            && (!language.word_comments
                || i == 0
                || matches!(bytes[i - 1], b' ' | b'\t' | b';' | b'(' | b'|' | b'&'))
        {
            out.span(i, bytes.len(), Token::Comment);
            return;
        }
        if language.rust_literals
            && let Some((end, token)) = rust_literal(language, bytes, i, state)
        {
            out.span(i, end, token);
            i = end;
            continue;
        }
        if let Some(quote) = quote_at(language, rest) {
            let start = i;
            let open = language.quotes[quote].open.len();
            i = string_end(language, bytes, i + open, quote as u8, 0, state);
            let key = language.keys == Keys::Colon && next_byte(bytes, i) == Some(b':');
            out.span(start, i, if key { Token::Property } else { Token::String });
            continue;
        }
        if byte.is_ascii_digit()
            || (byte == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            let start = i;
            i = number_end(bytes, i);
            out.span(start, i, Token::Number);
            continue;
        }
        if is_word_start(byte) {
            let start = i;
            i = word_end(language, bytes, i);
            let token = classify(language, bytes, start, i);
            if token == Token::Function && language.macros && bytes.get(i) == Some(&b'!') {
                i += 1;
            }
            out.span(start, i, token);
            continue;
        }
        if Some(byte) == language.decorator && bytes.get(i + 1).copied().is_some_and(is_word_start)
        {
            let start = i;
            i = word_end(language, bytes, i + 1);
            while bytes.get(i) == Some(&b'.')
                && bytes.get(i + 1).copied().is_some_and(is_word_start)
            {
                i = word_end(language, bytes, i + 1);
            }
            out.span(start, i, Token::Attribute);
            continue;
        }
        if language.variables && byte == b'$' {
            let start = i;
            i = variable_end(bytes, i);
            out.span(start, i, Token::Constant);
            continue;
        }
        i += 1;
    }
}

fn classify(language: &Language, bytes: &[u8], start: usize, end: usize) -> Token {
    let word = &bytes[start..end];
    let ignore_case = language.ignore_case;
    if language.string_prefixes
        && word.len() <= 2
        && word.iter().all(|b| b"bBrRfFuUL8".contains(b))
        && quote_at(language, &bytes[end..]).is_some()
    {
        return Token::String;
    }
    if holds(language.keywords, word, ignore_case) {
        return Token::Keyword;
    }
    if holds(language.constants, word, ignore_case) {
        return Token::Constant;
    }
    if holds(language.types, word, ignore_case) {
        return Token::Type;
    }
    if holds(language.builtins, word, ignore_case) {
        return Token::Function;
    }
    if language.macros && bytes.get(end) == Some(&b'!') && bytes.get(end + 1) != Some(&b'=') {
        return Token::Function;
    }
    let next = next_byte(bytes, end);
    if next == Some(b'(') {
        return Token::Function;
    }
    match language.keys {
        Keys::Assign if next == Some(b'=') && starts_line(bytes, start, false) => {
            return Token::Property;
        }
        Keys::Colon if next == Some(b':') && starts_line(bytes, start, true) => {
            return Token::Property;
        }
        _ => {}
    }
    if language.upper_types && word[0].is_ascii_uppercase() {
        let caps = word.len() > 1
            && word
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_');
        return if caps { Token::Constant } else { Token::Type };
    }
    Token::Plain
}

/// End of a block comment that is `depth` deep at `from`.
fn comment_end(
    language: &Language,
    bytes: &[u8],
    from: usize,
    depth: u8,
    state: &mut State,
) -> usize {
    let Some((open, close)) = language.block_comment else {
        *state = State::Normal;
        return bytes.len();
    };
    let (open, close) = (open.as_bytes(), close.as_bytes());
    let mut depth = depth;
    let mut i = from;
    while i < bytes.len() {
        let rest = &bytes[i..];
        if rest.starts_with(close) {
            i += close.len();
            depth = depth.saturating_sub(1);
            if depth == 0 {
                *state = State::Normal;
                return i;
            }
        } else if language.nested_comments && rest.starts_with(open) {
            i += open.len();
            depth = depth.saturating_add(1);
        } else {
            i += 1;
        }
    }
    *state = State::Comment(depth.max(1));
    bytes.len()
}

/// End of the string `quote` whose body starts at `from`.
fn string_end(
    language: &Language,
    bytes: &[u8],
    from: usize,
    quote: u8,
    hashes: u8,
    state: &mut State,
) -> usize {
    let mut i = from;
    if quote == RAW {
        let hashes_len = usize::from(hashes);
        while i < bytes.len() {
            if bytes[i] == b'"'
                && bytes
                    .get(i + 1..i + 1 + hashes_len)
                    .is_some_and(|tail| tail.iter().all(|b| *b == b'#'))
            {
                *state = State::Normal;
                return i + 1 + hashes_len;
            }
            i += 1;
        }
        *state = State::String { quote, hashes };
        return bytes.len();
    }
    let Some(spec) = language.quotes.get(usize::from(quote)) else {
        *state = State::Normal;
        return bytes.len();
    };
    let close = spec.close.as_bytes();
    while i < bytes.len() {
        if spec.escape && bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i..].starts_with(close) {
            *state = State::Normal;
            return i + close.len();
        }
        i += 1;
    }
    *state = if spec.multiline {
        State::String { quote, hashes: 0 }
    } else {
        State::Normal
    };
    bytes.len()
}

/// Rust's literals that the quote table cannot express. Returns the end and
/// token of the one at `i`, if any.
fn rust_literal(
    language: &Language,
    bytes: &[u8],
    i: usize,
    state: &mut State,
) -> Option<(usize, Token)> {
    let byte = bytes[i];
    match byte {
        b'#' if bytes[i + 1..].starts_with(b"[") || bytes[i + 1..].starts_with(b"![") => {
            let mut depth = 0usize;
            for (offset, b) in bytes[i..].iter().enumerate() {
                match b {
                    b'[' => depth += 1,
                    b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some((i + offset + 1, Token::Attribute));
                        }
                    }
                    _ => {}
                }
            }
            Some((bytes.len(), Token::Attribute))
        }
        b'\'' => {
            let next = *bytes.get(i + 1)?;
            if next == b'\\' {
                let end = bytes[i + 2..]
                    .iter()
                    .skip(1)
                    .position(|b| *b == b'\'')
                    .map_or(bytes.len(), |p| i + 2 + 1 + p + 1);
                return Some((end, Token::String));
            }
            let width = utf8_len(next);
            if bytes.get(i + 1 + width) == Some(&b'\'') {
                return Some((i + 2 + width, Token::String));
            }
            if is_word_start(next) {
                return Some((word_end(language, bytes, i + 1), Token::Attribute));
            }
            None
        }
        b'r' | b'b' => {
            if i > 0 && is_word(bytes[i - 1]) {
                return None;
            }
            let mut j = i + 1;
            if byte == b'b' {
                if bytes.get(j) != Some(&b'r') {
                    return None;
                }
                j += 1;
            }
            let hashes = bytes[j..].iter().take_while(|b| **b == b'#').count();
            if bytes.get(j + hashes) != Some(&b'"') {
                return None;
            }
            let hashes = u8::try_from(hashes).unwrap_or(u8::MAX);
            let end = string_end(
                language,
                bytes,
                j + usize::from(hashes) + 1,
                RAW,
                hashes,
                state,
            );
            Some((end, Token::String))
        }
        _ => None,
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xf0.. => 4,
        0xe0.. => 3,
        0xc0.. => 2,
        _ => 1,
    }
}

fn variable_end(bytes: &[u8], i: usize) -> usize {
    match bytes.get(i + 1) {
        Some(b'{') => bytes[i + 2..]
            .iter()
            .position(|b| *b == b'}')
            .map_or(bytes.len(), |p| i + 2 + p + 1),
        Some(b) if is_word(*b) => {
            let mut j = i + 1;
            while j < bytes.len() && is_word(bytes[j]) {
                j += 1;
            }
            j
        }
        Some(b) if b"?#@*!$-0123456789".contains(b) => i + 2,
        _ => i + 1,
    }
}

fn markdown<F: FnMut(usize, usize, Token)>(state: &mut State, bytes: &[u8], out: &mut Out<'_, F>) {
    let text = bytes.trim_ascii_start();
    let indent = bytes.len() - text.len();
    let fence = text.starts_with(b"```") || text.starts_with(b"~~~");
    if *state == State::Fence {
        if fence {
            *state = State::Normal;
            out.span(0, bytes.len(), Token::Comment);
        } else {
            out.span(0, bytes.len(), Token::String);
        }
        return;
    }
    *state = State::Normal;
    if fence {
        *state = State::Fence;
        out.span(0, bytes.len(), Token::Comment);
        return;
    }
    if text.starts_with(b"#") {
        out.span(0, bytes.len(), Token::Heading);
        return;
    }
    if text.starts_with(b">") {
        out.span(0, bytes.len(), Token::Comment);
        return;
    }
    let mut i = indent;
    if text.starts_with(b"- ") || text.starts_with(b"* ") || text.starts_with(b"+ ") {
        out.span(i, i + 1, Token::Keyword);
        i += 2;
    } else {
        let digits = text.iter().take_while(|b| b.is_ascii_digit()).count();
        if digits > 0
            && matches!(text.get(digits), Some(b'.' | b')'))
            && text.get(digits + 1) == Some(&b' ')
        {
            out.span(i, i + digits + 1, Token::Keyword);
            i += digits + 2;
        }
    }
    while i < bytes.len() {
        match bytes[i] {
            b'`' => {
                let end = bytes[i + 1..]
                    .iter()
                    .position(|b| *b == b'`')
                    .map_or(bytes.len(), |p| i + 1 + p + 1);
                out.span(i, end, Token::String);
                i = end;
            }
            b'*' | b'_' if bytes.get(i + 1) == Some(&bytes[i]) => {
                let marker = [bytes[i], bytes[i]];
                let end = bytes[i + 2..]
                    .windows(2)
                    .position(|w| w == marker)
                    .map_or(bytes.len(), |p| i + 2 + p + 2);
                out.span(i, end, Token::Constant);
                i = end;
            }
            b'[' => {
                let Some(close) = bytes[i..].iter().position(|b| *b == b']') else {
                    i += 1;
                    continue;
                };
                let close = i + close;
                out.span(i, close + 1, Token::Function);
                i = close + 1;
                if bytes.get(i) == Some(&b'(') {
                    let end = bytes[i..]
                        .iter()
                        .position(|b| *b == b')')
                        .map_or(bytes.len(), |p| i + p + 1);
                    out.span(i, end, Token::String);
                    i = end;
                }
            }
            _ => i += 1,
        }
    }
}

fn diff<F: FnMut(usize, usize, Token)>(bytes: &[u8], out: &mut Out<'_, F>) {
    let token = if bytes.starts_with(b"+++")
        || bytes.starts_with(b"---")
        || bytes.starts_with(b"diff ")
        || bytes.starts_with(b"index ")
    {
        Token::Keyword
    } else if bytes.starts_with(b"@@") {
        Token::Function
    } else if bytes.starts_with(b"+") {
        Token::String
    } else if bytes.starts_with(b"-") {
        Token::Attribute
    } else {
        Token::Plain
    };
    out.span(0, bytes.len(), token);
}

fn markup_name_end(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() && (is_word(bytes[i]) || matches!(bytes[i], b'-' | b':' | b'.')) {
        i += 1;
    }
    i
}

fn markup<F: FnMut(usize, usize, Token)>(state: &mut State, bytes: &[u8], out: &mut Out<'_, F>) {
    let mut i = 0;
    while i < bytes.len() {
        match *state {
            State::Comment(_) => {
                let start = i;
                i = match bytes[i..].windows(3).position(|w| w == b"-->") {
                    Some(p) => {
                        *state = State::Normal;
                        i + p + 3
                    }
                    None => bytes.len(),
                };
                out.span(start, i, Token::Comment);
            }
            State::Tag => {
                let byte = bytes[i];
                if byte == b'>' {
                    *state = State::Normal;
                    i += 1;
                } else if bytes[i..].starts_with(b"/>") {
                    *state = State::Normal;
                    i += 2;
                } else if byte == b'"' || byte == b'\'' {
                    let end = bytes[i + 1..]
                        .iter()
                        .position(|b| *b == byte)
                        .map_or(bytes.len(), |p| i + 1 + p + 1);
                    out.span(i, end, Token::String);
                    i = end;
                } else if is_word_start(byte) {
                    let end = markup_name_end(bytes, i);
                    out.span(i, end, Token::Property);
                    i = end;
                } else {
                    i += 1;
                }
            }
            _ => {
                *state = State::Normal;
                let byte = bytes[i];
                if bytes[i..].starts_with(b"<!--") {
                    *state = State::Comment(1);
                    out.span(i, i + 4, Token::Comment);
                    i += 4;
                } else if byte == b'<' {
                    let mut j = i + 1;
                    while matches!(bytes.get(j), Some(b'/' | b'!' | b'?')) {
                        j += 1;
                    }
                    if bytes.get(j).copied().is_some_and(is_word_start) {
                        let end = markup_name_end(bytes, j);
                        out.span(j, end, Token::Keyword);
                        *state = State::Tag;
                        i = end;
                    } else {
                        i += 1;
                    }
                } else if byte == b'&' {
                    let end = bytes[i + 1..]
                        .iter()
                        .take(12)
                        .position(|b| *b == b';')
                        .map(|p| i + 1 + p + 1);
                    match end {
                        Some(end) => {
                            out.span(i, end, Token::Constant);
                            i = end;
                        }
                        None => i += 1,
                    }
                } else {
                    i += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `line` split into (text, token) runs, merging neighbors of one token.
    fn runs(language: &Language, state: &mut State, line: &str) -> Vec<(String, Token)> {
        let mut out: Vec<(String, Token)> = Vec::new();
        let mut pos = 0;
        highlight(language, state, line, |start, end, token| {
            assert_eq!(start, pos, "ranges are contiguous");
            pos = end;
            match out.last_mut() {
                Some((text, last)) if *last == token => text.push_str(&line[start..end]),
                _ => out.push((line[start..end].to_owned(), token)),
            }
        });
        assert_eq!(pos, line.len(), "ranges cover the line");
        out
    }

    fn token_of(language: &Language, line: &str, word: &str) -> Token {
        let mut state = State::Normal;
        let at = line.find(word).unwrap();
        let mut found = None;
        highlight(language, &mut state, line, |start, end, token| {
            if start <= at && at < end {
                found = Some(token);
            }
        });
        found.unwrap()
    }

    #[test]
    fn word_lists_are_sorted_for_binary_search() {
        for language in LANGUAGES {
            for list in [
                language.keywords,
                language.types,
                language.constants,
                language.builtins,
            ] {
                for pair in list.windows(2) {
                    assert!(pair[0] < pair[1], "{}: {:?}", language.name, pair);
                }
                if language.ignore_case {
                    assert!(list.iter().all(|w| *w == w.to_lowercase()));
                }
            }
        }
    }

    #[test]
    fn rust_tokens() {
        let line = "pub fn main<'a>(x: &'a str) -> Option<u8> { println!(\"{}\", 42); None }";
        assert_eq!(token_of(&RUST, line, "pub"), Token::Keyword);
        assert_eq!(token_of(&RUST, line, "main"), Token::Plain);
        assert_eq!(token_of(&RUST, line, "'a>"), Token::Attribute);
        assert_eq!(token_of(&RUST, line, "str"), Token::Type);
        assert_eq!(token_of(&RUST, line, "Option"), Token::Type);
        assert_eq!(token_of(&RUST, line, "println!"), Token::Function);
        assert_eq!(token_of(&RUST, line, "\"{}\""), Token::String);
        assert_eq!(token_of(&RUST, line, "42"), Token::Number);
        assert_eq!(token_of(&RUST, line, "None"), Token::Constant);
        assert_eq!(token_of(&RUST, "let c = 'x';", "'x'"), Token::String);
        assert_eq!(
            token_of(&RUST, "#[derive(Debug)] struct", "derive"),
            Token::Attribute
        );
        assert_eq!(token_of(&RUST, "x(r#\"a\"b\"#)", "a"), Token::String);
        assert_eq!(token_of(&RUST, "f(b\"x\")", "b\""), Token::String);
        assert_eq!(token_of(&RUST, "call(1)", "call"), Token::Function);
    }

    #[test]
    fn block_comments_and_strings_carry_across_lines() {
        let mut state = State::Normal;
        let first = runs(&RUST, &mut state, "a /* one /* two */");
        assert_eq!(first.last().unwrap().1, Token::Comment);
        assert_eq!(state, State::Comment(1));
        let second = runs(&RUST, &mut state, "still */ b");
        assert_eq!(second[0], ("still */".to_owned(), Token::Comment));
        assert_eq!(state, State::Normal);

        let mut state = State::Normal;
        runs(&PYTHON, &mut state, "x = \"\"\"doc");
        assert!(matches!(state, State::String { .. }));
        let rest = runs(&PYTHON, &mut state, "end\"\"\" + y");
        assert_eq!(rest[0], ("end\"\"\"".to_owned(), Token::String));
        assert_eq!(state, State::Normal);

        let mut state = State::Normal;
        runs(&RUST, &mut state, "let s = r#\"raw");
        assert!(matches!(
            state,
            State::String {
                quote: RAW,
                hashes: 1
            }
        ));
        runs(&RUST, &mut state, "\"# ;");
        assert_eq!(state, State::Normal);
    }

    #[test]
    fn config_keys_sections_and_comments() {
        assert_eq!(token_of(&TOML, "[package]", "package"), Token::Type);
        assert_eq!(token_of(&TOML, "name = \"x\"", "name"), Token::Property);
        assert_eq!(token_of(&TOML, "name = \"x\" # c", "# c"), Token::Comment);
        assert_eq!(token_of(&JSON, "  \"key\": 1,", "\"key\""), Token::Property);
        assert_eq!(token_of(&JSON, "  \"key\": \"v\",", "\"v\""), Token::String);
        assert_eq!(
            token_of(&YAML, "- run-name: echo", "run-name"),
            Token::Property
        );
        assert_eq!(token_of(&YAML, "url: a#b", "a#b"), Token::Plain);
        assert_eq!(
            token_of(&SHELL, "echo $HOME ${x} # hi", "$HOME"),
            Token::Constant
        );
        assert_eq!(
            token_of(&SHELL, "echo $HOME ${x} # hi", "${x}"),
            Token::Constant
        );
        assert_eq!(
            token_of(&SHELL, "echo $HOME ${x} # hi", "# hi"),
            Token::Comment
        );
        assert_eq!(token_of(&SQL, "SELECT id FROM t", "SELECT"), Token::Keyword);
        assert_eq!(token_of(&SQL, "select id from t", "from"), Token::Keyword);
    }

    #[test]
    fn markup_markdown_and_diff() {
        let mut state = State::Normal;
        let line = "<a href=\"x\">&amp; <!-- c";
        assert_eq!(token_of(&HTML, line, "a "), Token::Keyword);
        assert_eq!(token_of(&HTML, line, "href"), Token::Property);
        assert_eq!(token_of(&HTML, line, "\"x\""), Token::String);
        assert_eq!(token_of(&HTML, line, "&amp;"), Token::Constant);
        runs(&HTML, &mut state, line);
        assert_eq!(state, State::Comment(1));
        runs(&HTML, &mut state, "-->");
        assert_eq!(state, State::Normal);

        let mut state = State::Normal;
        assert_eq!(runs(&MARKDOWN, &mut state, "# Title")[0].1, Token::Heading);
        runs(&MARKDOWN, &mut state, "```rust");
        assert_eq!(state, State::Fence);
        assert_eq!(runs(&MARKDOWN, &mut state, "let x;")[0].1, Token::String);
        runs(&MARKDOWN, &mut state, "```");
        assert_eq!(state, State::Normal);
        assert_eq!(
            token_of(&MARKDOWN, "see `code` now", "`code`"),
            Token::String
        );

        assert_eq!(token_of(&DIFF, "+added", "added"), Token::String);
        assert_eq!(token_of(&DIFF, "-gone", "gone"), Token::Attribute);
        assert_eq!(token_of(&DIFF, "@@ -1 +1 @@", "@@"), Token::Function);
    }

    #[test]
    fn non_ascii_text_never_splits_a_char() {
        for language in LANGUAGES {
            let mut state = State::Normal;
            for line in [
                "größe = \"ü\" // ß",
                "'é' 日本 #[x] <ä> **ö**",
                "'\\",
                "\"\\",
            ] {
                highlight(language, &mut state, line, |start, end, _| {
                    assert!(line.is_char_boundary(start) && line.is_char_boundary(end));
                });
            }
        }
    }

    #[test]
    fn detects_by_name_extension_and_interpreter() {
        let name = |path: &str, first: &[u8]| detect(Path::new(path), first).map(|l| l.name);
        assert_eq!(name("src/main.rs", b""), Some("Rust"));
        assert_eq!(name("a/B.PY", b""), Some("Python"));
        assert_eq!(name("Makefile", b""), Some("Makefile"));
        assert_eq!(name("Dockerfile.dev", b""), Some("Dockerfile"));
        assert_eq!(name("bin/run", b"#!/usr/bin/env bash"), Some("Shell"));
        assert_eq!(name("bin/run", b"#!/usr/bin/python3 -u"), Some("Python"));
        assert_eq!(name("LICENSE", b"MIT"), None);
    }
}
