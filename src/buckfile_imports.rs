/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Minimal parser for the Starlark statements in `buck.buckfile_imports`.
//!
//! In split mode, every generated BUCK file receives only those statements
//! from `buckfile_imports` which the file's rules transitively depend on.
//! Top-level expression statements, such as `oncall("...")`, are assumed to
//! have side effects and are always kept. Assignments are kept if a kept
//! statement or generated rule refers to the assigned name. Load statements
//! are trimmed down to the symbols that are referred to.

use anyhow::bail;

#[derive(Clone, Debug, Default)]
pub struct BuckfileImports {
    statements: Vec<Statement>,
    trailer: Option<Trailer>,
}

#[derive(Clone, Debug)]
struct Statement {
    /// Whether there is a blank line between this statement (including its
    /// leading comments) and the previous statement.
    blank_line_before: bool,
    /// Comments preceding the statement, verbatim.
    leading: String,
    /// Source text of the statement, including a comment at the end of its
    /// last line.
    code: String,
    kind: StatementKind,
}

/// Comments after the last statement.
#[derive(Clone, Debug)]
struct Trailer {
    blank_line_before: bool,
    text: String,
}

#[derive(Clone, Debug, PartialEq)]
enum StatementKind {
    Load(Load),
    Assign { target: String, value: Expr },
    Expr(Expr),
}

#[derive(Clone, Debug, PartialEq)]
struct Load {
    /// String literal, as written in the source.
    module: String,
    symbols: Vec<LoadSymbol>,
}

#[derive(Clone, Debug, PartialEq)]
struct LoadSymbol {
    /// Name bound in the BUCK file.
    local: String,
    /// String literal naming the symbol in the loaded module, as written in
    /// the source.
    original: String,
}

#[derive(Clone, Debug, PartialEq)]
enum Expr {
    /// String literal, as written in the source.
    String(String),
    Number(String),
    Identifier(String),
    Attribute(Box<Expr>, String),
    Call(Box<Expr>, Vec<Argument>),
    List(Vec<Expr>),
    Dict(Vec<(Expr, Expr)>),
}

#[derive(Clone, Debug, PartialEq)]
struct Argument {
    name: Option<String>,
    value: Expr,
}

impl BuckfileImports {
    pub fn parse(src: &str) -> anyhow::Result<Self> {
        let tokens = tokenize(src)?;
        let mut parser = Parser {
            src,
            tokens,
            pos: 0,
        };

        let mut statements = Vec::new();
        let mut prev_end = 0;
        loop {
            parser.skip_newlines();
            let first = parser.peek();
            if first.kind == TokenKind::Eof {
                break;
            }
            let kind = parser.statement()?;
            let next = parser.peek();
            if !matches!(next.kind, TokenKind::Newline | TokenKind::Eof) {
                return Err(parser.unexpected(next, "end of statement"));
            }
            let last_end = parser.tokens[parser.pos - 1].end;
            let end = end_of_trailing_comment(src, last_end);
            let leading_start = skip_whitespace(src, prev_end);
            statements.push(Statement {
                blank_line_before: has_blank_line(&src[prev_end..leading_start]),
                leading: src[leading_start..first.start].to_owned(),
                code: src[first.start..end].to_owned(),
                kind,
            });
            prev_end = end;
        }

        let trailer_start = skip_whitespace(src, prev_end);
        let trailer = (trailer_start < src.len()).then(|| Trailer {
            blank_line_before: has_blank_line(&src[prev_end..trailer_start]),
            text: src[trailer_start..].trim_end().to_owned(),
        });

        Ok(BuckfileImports {
            statements,
            trailer,
        })
    }

    /// Render the statements needed by a BUCK file whose generated rules refer
    /// to the given top-level names.
    pub fn render<'a>(&self, used: impl IntoIterator<Item = &'a str>) -> String {
        let mut needed_statements = vec![false; self.statements.len()];
        let mut needed_symbols: Vec<Vec<bool>> = self
            .statements
            .iter()
            .map(|stmt| match &stmt.kind {
                StatementKind::Load(load) => vec![false; load.symbols.len()],
                StatementKind::Assign { .. } | StatementKind::Expr(_) => Vec::new(),
            })
            .collect();

        // Pairs of (name, index). The name refers to the latest definition
        // among the statements preceding index.
        let mut worklist = Vec::new();
        for name in used {
            worklist.push((name, self.statements.len()));
        }
        for (i, stmt) in self.statements.iter().enumerate() {
            if let StatementKind::Expr(expr) = &stmt.kind {
                needed_statements[i] = true;
                expr.for_each_reference(&mut |name| worklist.push((name, i)));
            }
        }

        while let Some((name, before)) = worklist.pop() {
            for (i, stmt) in self.statements[..before].iter().enumerate().rev() {
                match &stmt.kind {
                    StatementKind::Load(load) => {
                        if let Some(j) = load.symbols.iter().position(|sym| sym.local == name) {
                            needed_statements[i] = true;
                            needed_symbols[i][j] = true;
                            break;
                        }
                    }
                    StatementKind::Assign { target, value } => {
                        if target == name {
                            if !needed_statements[i] {
                                needed_statements[i] = true;
                                value.for_each_reference(&mut |name| worklist.push((name, i)));
                            }
                            break;
                        }
                    }
                    StatementKind::Expr(_) => {}
                }
            }
        }

        let mut out = String::new();
        let mut blank_line_pending = false;
        for (i, stmt) in self.statements.iter().enumerate() {
            blank_line_pending |= stmt.blank_line_before;
            if !needed_statements[i] {
                continue;
            }
            if !out.is_empty() {
                out.push_str(if blank_line_pending { "\n\n" } else { "\n" });
            }
            blank_line_pending = false;
            out.push_str(&stmt.leading);
            match &stmt.kind {
                StatementKind::Load(load) if needed_symbols[i].contains(&false) => {
                    let symbols = load
                        .symbols
                        .iter()
                        .zip(&needed_symbols[i])
                        .filter_map(|(sym, needed)| needed.then_some(sym));
                    let multiline = stmt.code.contains('\n');
                    render_load(&mut out, &load.module, symbols, multiline);
                }
                _ => out.push_str(&stmt.code),
            }
        }
        if let Some(trailer) = &self.trailer {
            if !out.is_empty() {
                out.push_str(if blank_line_pending || trailer.blank_line_before {
                    "\n\n"
                } else {
                    "\n"
                });
            }
            out.push_str(&trailer.text);
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }
}

fn render_load<'a>(
    out: &mut String,
    module: &str,
    symbols: impl Iterator<Item = &'a LoadSymbol>,
    multiline: bool,
) {
    let (open, separator, close) = if multiline {
        ("(\n    ", ",\n    ", ",\n)")
    } else {
        ("(", ", ", ")")
    };
    out.push_str("load");
    out.push_str(open);
    out.push_str(module);
    for sym in symbols {
        out.push_str(separator);
        if string_value(&sym.original) != Some(sym.local.as_str()) {
            out.push_str(&sym.local);
            out.push_str(" = ");
        }
        out.push_str(&sym.original);
    }
    out.push_str(close);
}

impl Expr {
    /// Visit every free identifier referenced by this expression.
    fn for_each_reference<'a>(&'a self, f: &mut impl FnMut(&'a str)) {
        match self {
            Expr::String(_) | Expr::Number(_) => {}
            Expr::Identifier(name) => f(name),
            Expr::Attribute(object, _attr) => object.for_each_reference(f),
            Expr::Call(function, args) => {
                function.for_each_reference(f);
                for arg in args {
                    arg.value.for_each_reference(f);
                }
            }
            Expr::List(elements) => {
                for element in elements {
                    element.for_each_reference(f);
                }
            }
            Expr::Dict(entries) => {
                for (key, value) in entries {
                    key.for_each_reference(f);
                    value.for_each_reference(f);
                }
            }
        }
    }
}

/// Contents of a plain string literal without escapes or prefixes.
fn string_value(literal: &str) -> Option<&str> {
    let inner = literal
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| literal.strip_prefix('\'')?.strip_suffix('\''))?;
    (!inner.is_empty() && !inner.contains(['\\', '"', '\''])).then_some(inner)
}

fn skip_whitespace(src: &str, offset: usize) -> usize {
    let rest = &src[offset..];
    offset + (rest.len() - rest.trim_start().len())
}

fn has_blank_line(whitespace: &str) -> bool {
    whitespace.matches('\n').count() >= 2
}

fn end_of_trailing_comment(src: &str, offset: usize) -> usize {
    let rest = &src[offset..];
    let after_spaces = rest.trim_start_matches([' ', '\t']);
    if after_spaces.starts_with('#') {
        offset + rest.find('\n').unwrap_or(rest.len())
    } else {
        offset
    }
}

fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let before = &src[..offset];
    let line = before.matches('\n').count() + 1;
    let col = before.len() - before.rfind('\n').map_or(0, |i| i + 1) + 1;
    (line, col)
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum TokenKind {
    Identifier,
    String,
    Number,
    Punct(u8),
    Newline,
    Eof,
}

#[derive(Copy, Clone, Debug)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}

impl Token {
    fn is_punct(&self, ch: u8) -> bool {
        self.kind == TokenKind::Punct(ch)
    }
}

fn tokenize(src: &str) -> anyhow::Result<Vec<Token>> {
    let bytes = src.as_bytes();
    let mut tokens = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let kind = match bytes[i] {
            b' ' | b'\t' | b'\r' => {
                i += 1;
                continue;
            }
            b'\\' if bytes.get(i + 1) == Some(&b'\n') => {
                i += 2;
                continue;
            }
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'\n' => {
                i += 1;
                if depth > 0
                    || tokens
                        .last()
                        .is_none_or(|tok: &Token| tok.kind == TokenKind::Newline)
                {
                    continue;
                }
                TokenKind::Newline
            }
            b'"' | b'\'' => {
                i = end_of_string(src, start, i)?;
                TokenKind::String
            }
            b'0'..=b'9' => {
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.') {
                    i += 1;
                }
                TokenKind::Number
            }
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if matches!(&src[start..i], "r" | "b" | "rb" | "br")
                    && matches!(bytes.get(i), Some(b'"' | b'\''))
                {
                    i = end_of_string(src, start, i)?;
                    TokenKind::String
                } else {
                    TokenKind::Identifier
                }
            }
            ch @ (b'(' | b'[' | b'{') => {
                i += 1;
                depth += 1;
                TokenKind::Punct(ch)
            }
            ch @ (b')' | b']' | b'}') => {
                i += 1;
                let Some(new_depth) = depth.checked_sub(1) else {
                    let (line, col) = line_col(src, start);
                    bail!("unbalanced `{}` at line {line}, column {col}", ch as char);
                };
                depth = new_depth;
                TokenKind::Punct(ch)
            }
            ch @ (b',' | b'=' | b':' | b'.' | b'-') => {
                i += 1;
                TokenKind::Punct(ch)
            }
            _ => {
                let (line, col) = line_col(src, start);
                let ch = src[start..]
                    .chars()
                    .next()
                    .expect("should be at a char boundary");
                bail!("unsupported character {ch:?} at line {line}, column {col}");
            }
        };
        tokens.push(Token {
            kind,
            start,
            end: i,
        });
    }
    tokens.push(Token {
        kind: TokenKind::Eof,
        start: src.len(),
        end: src.len(),
    });
    Ok(tokens)
}

/// Returns the offset one past the closing quote of the string literal whose
/// opening quote is at `quote`.
fn end_of_string(src: &str, start: usize, quote: usize) -> anyhow::Result<usize> {
    let bytes = src.as_bytes();
    let q = bytes[quote];
    let triple = bytes[quote..].starts_with(&[q, q, q]);
    let mut i = quote + if triple { 3 } else { 1 };
    loop {
        match bytes.get(i) {
            None => break,
            Some(b'\n') if !triple => break,
            Some(b'\\') => i += 2,
            Some(&ch) if ch == q => {
                if !triple {
                    return Ok(i + 1);
                } else if bytes[i..].starts_with(&[q, q, q]) {
                    return Ok(i + 3);
                }
                i += 1;
            }
            Some(_) => i += 1,
        }
    }
    let (line, col) = line_col(src, start);
    bail!("unterminated string literal at line {line}, column {col}");
}

struct Parser<'a> {
    src: &'a str,
    tokens: Vec<Token>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Token {
        self.peek_nth(0)
    }

    fn peek_nth(&self, n: usize) -> Token {
        self.tokens[(self.pos + n).min(self.tokens.len() - 1)]
    }

    fn next(&mut self) -> Token {
        let tok = self.peek();
        if tok.kind != TokenKind::Eof {
            self.pos += 1;
        }
        tok
    }

    fn text(&self, tok: Token) -> &'a str {
        &self.src[tok.start..tok.end]
    }

    fn skip_newlines(&mut self) {
        while self.peek().kind == TokenKind::Newline {
            self.pos += 1;
        }
    }

    fn eat_punct(&mut self, ch: u8) -> bool {
        let eat = self.peek().is_punct(ch);
        if eat {
            self.pos += 1;
        }
        eat
    }

    fn expect_punct(&mut self, ch: u8) -> anyhow::Result<()> {
        let tok = self.next();
        if tok.is_punct(ch) {
            Ok(())
        } else {
            Err(self.unexpected(tok, &format!("`{}`", ch as char)))
        }
    }

    fn expect(&mut self, kind: TokenKind, expected: &str) -> anyhow::Result<&'a str> {
        let tok = self.next();
        if tok.kind == kind {
            Ok(self.text(tok))
        } else {
            Err(self.unexpected(tok, expected))
        }
    }

    fn unexpected(&self, tok: Token, expected: &str) -> anyhow::Error {
        let (line, col) = line_col(self.src, tok.start);
        let found = match tok.kind {
            TokenKind::Newline => "end of line".to_owned(),
            TokenKind::Eof => "end of input".to_owned(),
            _ => format!("`{}`", self.text(tok)),
        };
        anyhow::anyhow!("expected {expected}, found {found} at line {line}, column {col}")
    }

    fn statement(&mut self) -> anyhow::Result<StatementKind> {
        let first = self.peek();
        if first.kind == TokenKind::Identifier {
            let second = self.peek_nth(1);
            if self.text(first) == "load" && second.is_punct(b'(') {
                self.pos += 2;
                return self.load().map(StatementKind::Load);
            }
            if second.is_punct(b'=') {
                self.pos += 2;
                let target = self.text(first).to_owned();
                let value = self.expr()?;
                return Ok(StatementKind::Assign { target, value });
            }
        }
        self.expr().map(StatementKind::Expr)
    }

    fn load(&mut self) -> anyhow::Result<Load> {
        let module = self.expect(TokenKind::String, "module string")?.to_owned();
        let mut symbols = Vec::new();
        while self.eat_punct(b',') {
            if self.peek().is_punct(b')') {
                break;
            }
            let tok = self.next();
            let (local, original) = match tok.kind {
                TokenKind::String => (None, tok),
                TokenKind::Identifier => {
                    self.expect_punct(b'=')?;
                    let original = self.next();
                    if original.kind != TokenKind::String {
                        return Err(self.unexpected(original, "symbol string"));
                    }
                    (Some(self.text(tok)), original)
                }
                _ => return Err(self.unexpected(tok, "symbol to load")),
            };
            let Some(name) = string_value(self.text(original)) else {
                return Err(self.unexpected(original, "plain string literal"));
            };
            symbols.push(LoadSymbol {
                local: local.unwrap_or(name).to_owned(),
                original: self.text(original).to_owned(),
            });
        }
        if symbols.is_empty() {
            let tok = self.next();
            return Err(self.unexpected(tok, "symbol to load"));
        }
        self.expect_punct(b')')?;
        Ok(Load { module, symbols })
    }

    fn expr(&mut self) -> anyhow::Result<Expr> {
        let mut expr = self.primary()?;
        loop {
            if self.eat_punct(b'.') {
                let attr = self.expect(TokenKind::Identifier, "attribute name")?;
                expr = Expr::Attribute(Box::new(expr), attr.to_owned());
            } else if self.eat_punct(b'(') {
                let args = self.sequence(b')', Self::argument)?;
                expr = Expr::Call(Box::new(expr), args);
            } else {
                return Ok(expr);
            }
        }
    }

    fn primary(&mut self) -> anyhow::Result<Expr> {
        let tok = self.next();
        let text = self.text(tok).to_owned();
        match tok.kind {
            TokenKind::Identifier => Ok(Expr::Identifier(text)),
            TokenKind::String => Ok(Expr::String(text)),
            TokenKind::Number => Ok(Expr::Number(text)),
            TokenKind::Punct(b'-') => {
                let number = self.next();
                if number.kind != TokenKind::Number {
                    return Err(self.unexpected(number, "number"));
                }
                Ok(Expr::Number(self.src[tok.start..number.end].to_owned()))
            }
            TokenKind::Punct(b'[') => self.sequence(b']', Self::expr).map(Expr::List),
            TokenKind::Punct(b'{') => self.sequence(b'}', Self::dict_entry).map(Expr::Dict),
            TokenKind::Punct(b'(') => {
                let expr = self.expr()?;
                self.expect_punct(b')')?;
                Ok(expr)
            }
            _ => Err(self.unexpected(tok, "expression")),
        }
    }

    fn argument(&mut self) -> anyhow::Result<Argument> {
        let first = self.peek();
        let name = if first.kind == TokenKind::Identifier && self.peek_nth(1).is_punct(b'=') {
            self.pos += 2;
            Some(self.text(first).to_owned())
        } else {
            None
        };
        let value = self.expr()?;
        Ok(Argument { name, value })
    }

    fn dict_entry(&mut self) -> anyhow::Result<(Expr, Expr)> {
        let key = self.expr()?;
        self.expect_punct(b':')?;
        let value = self.expr()?;
        Ok((key, value))
    }

    /// Comma-separated elements, with optional trailing comma, up to and
    /// including the closing delimiter.
    fn sequence<T>(
        &mut self,
        close: u8,
        mut element: impl FnMut(&mut Self) -> anyhow::Result<T>,
    ) -> anyhow::Result<Vec<T>> {
        let mut elements = Vec::new();
        loop {
            if self.eat_punct(close) {
                return Ok(elements);
            }
            elements.push(element(self)?);
            if !self.eat_punct(b',') {
                self.expect_punct(close)?;
                return Ok(elements);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    const THIRD_PARTY_RUST: &str = indoc! {r#"
        load(
            "@fbsource//third-party/rust:disallowed_crates.bzl",
            "disallowed_top_level_crates_wrapper",
        )
        load(
            "@fbsource//tools/build_defs/third_party:rust_third_party.bzl",
            "rust_buildscript_genrule",
            "third_party_filegroup",
            "third_party_rust_binary",
            "third_party_rust_library",
            _third_party_alias = "third_party_alias",
        )

        oncall("rust_foundation")

        third_party_alias = disallowed_top_level_crates_wrapper(_third_party_alias)
    "#};

    #[test]
    fn library_only() {
        let imports = BuckfileImports::parse(THIRD_PARTY_RUST).unwrap();
        let expected = indoc! {r#"
            load(
                "@fbsource//tools/build_defs/third_party:rust_third_party.bzl",
                "third_party_rust_library",
            )

            oncall("rust_foundation")
        "#};
        assert_eq!(imports.render(["third_party_rust_library"]), expected);
    }

    #[test]
    fn alias_through_assignment() {
        let imports = BuckfileImports::parse(THIRD_PARTY_RUST).unwrap();
        let expected = indoc! {r#"
            load(
                "@fbsource//third-party/rust:disallowed_crates.bzl",
                "disallowed_top_level_crates_wrapper",
            )
            load(
                "@fbsource//tools/build_defs/third_party:rust_third_party.bzl",
                "third_party_filegroup",
                _third_party_alias = "third_party_alias",
            )

            oncall("rust_foundation")

            third_party_alias = disallowed_top_level_crates_wrapper(_third_party_alias)
        "#};
        assert_eq!(
            imports.render(["third_party_alias", "third_party_filegroup", "select"]),
            expected,
        );
    }

    #[test]
    fn everything_used_is_verbatim() {
        let imports = BuckfileImports::parse(THIRD_PARTY_RUST).unwrap();
        let used = [
            "rust_buildscript_genrule",
            "third_party_filegroup",
            "third_party_rust_binary",
            "third_party_rust_library",
            "third_party_alias",
        ];
        assert_eq!(imports.render(used), THIRD_PARTY_RUST);
    }

    #[test]
    fn single_line_loads_and_comments() {
        let src = indoc! {r#"
            load("@prelude//rust:cargo_buildscript.bzl", "buildscript_run")
            load("@prelude//rust:cargo_package.bzl", "alias", "cargo")  # trailing
            load("@fbcode_macros//build_defs:native_rules.bzl", fbcode_alias = "alias")

            # Comment attached to the call.
            fbcode_alias(
                name = "intent-core",
                actual = ":tbd-intent-0.1-intent",
                visibility = ["PUBLIC"],
            )

            # Trailing comment.
        "#};
        let imports = BuckfileImports::parse(src).unwrap();
        let expected = indoc! {r#"
            load("@prelude//rust:cargo_package.bzl", "cargo")
            load("@fbcode_macros//build_defs:native_rules.bzl", fbcode_alias = "alias")

            # Comment attached to the call.
            fbcode_alias(
                name = "intent-core",
                actual = ":tbd-intent-0.1-intent",
                visibility = ["PUBLIC"],
            )

            # Trailing comment.
        "#};
        assert_eq!(imports.render(["cargo"]), expected);
    }

    #[test]
    fn shadowing() {
        let src = indoc! {r#"
            load(":a.bzl", "f")
            load(":b.bzl", "g")
            g = f(g)
            load(":c.bzl", "f")
        "#};
        let imports = BuckfileImports::parse(src).unwrap();
        let expected = indoc! {r#"
            load(":a.bzl", "f")
            load(":b.bzl", "g")
            g = f(g)
        "#};
        assert_eq!(imports.render(["g"]), expected);
        assert_eq!(imports.render(["f"]), "load(\":c.bzl\", \"f\")\n");
        assert_eq!(imports.render(["h"]), "");
    }

    #[test]
    fn structure() {
        let src = indoc! {r#"
            x = cargo.rust_library(
                "pos",
                n = -1,
                flag = False,
                platform = {"linux": dict(deps = [":libc-0.2"])},
                cmd = "sed '/^#!\[/d' $OUT",
                doc = '''multi
            line''',
            )
        "#};
        let imports = BuckfileImports::parse(src).unwrap();
        let [stmt] = imports.statements.as_slice() else {
            panic!("expected 1 statement");
        };
        let ident = |name: &str| Expr::Identifier(name.to_owned());
        let string = |lit: &str| Expr::String(lit.to_owned());
        let arg = |name: &str, value| Argument {
            name: Some(name.to_owned()),
            value,
        };
        let expected = StatementKind::Assign {
            target: "x".to_owned(),
            value: Expr::Call(
                Box::new(Expr::Attribute(
                    Box::new(ident("cargo")),
                    "rust_library".to_owned(),
                )),
                vec![
                    Argument {
                        name: None,
                        value: string("\"pos\""),
                    },
                    arg("n", Expr::Number("-1".to_owned())),
                    arg("flag", ident("False")),
                    arg(
                        "platform",
                        Expr::Dict(vec![(
                            string("\"linux\""),
                            Expr::Call(
                                Box::new(ident("dict")),
                                vec![arg("deps", Expr::List(vec![string("\":libc-0.2\"")]))],
                            ),
                        )]),
                    ),
                    arg("cmd", string(r#""sed '/^#!\[/d' $OUT""#)),
                    arg("doc", string("'''multi\nline'''")),
                ],
            ),
        };
        assert_eq!(stmt.kind, expected);
    }

    #[test]
    fn errors() {
        let err = BuckfileImports::parse("load(\":a.bzl\")\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "expected symbol to load, found `)` at line 1, column 14",
        );
        let err = BuckfileImports::parse("x = 1 + 2\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "unsupported character '+' at line 1, column 7",
        );
        let err = BuckfileImports::parse("f(\n  \"abc\n)\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "unterminated string literal at line 2, column 3",
        );
        let err = BuckfileImports::parse("f() g()\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "expected end of statement, found `g` at line 1, column 5",
        );
    }
}
