/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Search source files used to compile a Rust crate.
//!
//! This follows the Rust Reference book on [Modules] and takes some
//! liberties to parse certain well-known macros like `cfg_if`.
//!
//! [Modules]: https://doc.rust-lang.org/reference/items/modules.html

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use foldhash::HashSet;
use proc_macro2::TokenStream;
use syn::Token;
use syn::ext::IdentExt as _;
use syn::visit;
use syn::visit::Visit;

use crate::path::normalized_extend_path;

#[allow(dead_code)]
#[derive(Debug)]
pub struct Error {
    path: PathBuf,
    kind: ErrorKind,
}

#[derive(Debug)]
pub enum ErrorKind {
    FileError {
        #[allow(dead_code)]
        source_path: PathBuf,
        source: io::Error,
    },
    IncludeNotFound {
        #[allow(dead_code)]
        source_path: PathBuf,
    },
    ModuleNotFound {
        #[allow(dead_code)]
        default_path: PathBuf,
        #[allow(dead_code)]
        secondary_path: Option<PathBuf>,
    },
    ParserError {
        #[allow(dead_code)]
        source_path: PathBuf,
        source: syn::Error,
        #[allow(dead_code)]
        line: usize,
    },
}

impl Error {
    fn new(path: impl Into<PathBuf>, kind: ErrorKind) -> Self {
        Error {
            path: path.into(),
            kind,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self, f)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self.kind {
            ErrorKind::FileError { ref source, .. } => Some(source),
            ErrorKind::IncludeNotFound { .. } => None,
            ErrorKind::ModuleNotFound { .. } => None,
            ErrorKind::ParserError { ref source, .. } => Some(source),
        }
    }
}

pub fn crate_srcfiles(path: impl AsRef<Path>) -> Sources {
    let path = path.as_ref();

    let mut sources = Sources {
        files: HashSet::default(),
        errors: vec![],
    };

    let mod_rs = ModRs::Yes; // Entry files are presumed to be "mod-rs" files.

    SourceFinder {
        current: path,
        sources: &mut sources,
        mod_ancestors: vec![],
        mod_rs,
    }
    .parse_and_visit_source_file(path, mod_rs);

    sources
}

#[derive(Debug)]
pub struct Sources {
    pub files: HashSet<PathBuf>,
    pub errors: Vec<Error>,
}

#[derive(Copy, Clone, Debug)]
enum ModRs {
    Yes,
    Auto,
}

#[derive(Debug)]
struct SourceFinder<'s> {
    current: &'s Path,
    sources: &'s mut Sources,
    mod_ancestors: Vec<String>,
    mod_rs: ModRs,
}

impl SourceFinder<'_> {
    fn mod_parent_dir(&self) -> PathBuf {
        let is_mod_rs = match self.mod_rs {
            ModRs::Yes => true,
            ModRs::Auto => match self.current.file_name() {
                Some(x) => x == "lib.rs" || x == "main.rs" || x == "mod.rs",
                None => false,
            },
        };
        if is_mod_rs {
            parent_dir(self.current).to_owned()
        } else {
            self.current.with_extension("")
        }
    }

    fn visit_cfg_if(&mut self, node: &CfgIf) {
        self.visit_block(&node.then_branch);
        match node.else_branch.as_deref() {
            Some(CfgExpr::Block(block)) => self.visit_block(block),
            Some(CfgExpr::If(cfg_if)) => self.visit_cfg_if(cfg_if),
            None => {}
        }
    }

    fn collect_relative_path(&mut self, path: &syn::LitStr) -> Result<(), ErrorKind> {
        let source_path = {
            let mut p = parent_dir(self.current).to_owned();
            normalized_extend_path(&mut p, path.value());
            p
        };
        match fs::File::open(&source_path) {
            Ok(_) => {
                self.sources.files.insert(source_path);
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                Err(ErrorKind::IncludeNotFound { source_path })
            }
            Err(err) => Err(ErrorKind::FileError {
                source_path,
                source: err,
            }),
        }
    }

    fn push_error(&mut self, kind: ErrorKind) {
        self.sources.errors.push(Error {
            path: self.current.to_owned(),
            kind,
        });
    }

    /// Returns `true` if something was added to `sources`. In other words,
    /// returns `false` if the source file was not found.
    fn parse_and_visit_source_file(&mut self, source_path: &Path, mod_rs: ModRs) -> bool {
        match fs::read_to_string(source_path) {
            Ok(content) => {
                // rustc does not allow circular modules. But we check for it
                // in case there's a bad package out there.
                if self.sources.files.contains(source_path) {
                    return true;
                }
                self.sources.files.insert(source_path.to_owned());
                match syn::parse_file(&content) {
                    Ok(ast) => {
                        SourceFinder {
                            current: source_path,
                            sources: self.sources,
                            mod_ancestors: vec![],
                            mod_rs,
                        }
                        .visit_file(&ast);
                    }
                    Err(err) => {
                        self.sources.errors.push(Error::new(
                            self.current,
                            ErrorKind::ParserError {
                                line: err.span().start().line,
                                source_path: source_path.to_owned(),
                                source: err,
                            },
                        ));
                    }
                };
                true
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => false,
            Err(err) => {
                self.sources.errors.push(Error::new(
                    self.current,
                    ErrorKind::FileError {
                        source_path: source_path.to_owned(),
                        source: err,
                    },
                ));
                true
            }
        }
    }

    fn parse_debugger_visualizer(&mut self, attrs: &[syn::Attribute]) {
        // Look for #![debugger_visualizer(natvis_file = "...")].
        // https://doc.rust-lang.org/1.91.0/reference/attributes/debugger.html#the-debugger_visualizer-attribute
        for attr in attrs {
            if let syn::Meta::List(meta) = &attr.meta
                && meta.path.is_ident("debugger_visualizer")
            {
                let _ = meta.parse_nested_meta(|nested| {
                    if nested.path.is_ident("natvis_file")
                        || nested.path.is_ident("gdb_script_file")
                    {
                        let lit: syn::LitStr = nested.value()?.parse()?;
                        if let Err(err) = self.collect_relative_path(&lit) {
                            self.push_error(err);
                        }
                    }
                    Ok(())
                });
            }
        }
    }
}

fn parent_dir(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new(".."))
}

fn cfg_test(attrs: &[syn::Attribute]) -> bool {
    // Look for #[cfg(test)] and #[cfg(bench)].
    for attr in attrs {
        let mut is_cfg_test = None;
        if let syn::Meta::List(meta) = &attr.meta
            && meta.path.is_ident("cfg")
            && meta
                .parse_nested_meta(|cfg| {
                    if cfg.path.is_ident("test") || cfg.path.is_ident("bench") {
                        is_cfg_test.get_or_insert(true);
                    } else if cfg.path.is_ident("all") {
                        // Look for #[cfg(all(test, ...))].
                        let _ = cfg.parse_nested_meta(|all| {
                            if all.path.is_ident("test") || all.path.is_ident("bench") {
                                is_cfg_test.get_or_insert(true);
                            } else {
                                ignore_nested_meta(all.input)?;
                            }
                            Ok(())
                        });
                    } else {
                        is_cfg_test = Some(false);
                    }
                    Ok(())
                })
                .is_ok()
            && is_cfg_test == Some(true)
        {
            return true;
        }
    }
    false
}

impl<'ast> Visit<'ast> for SourceFinder<'_> {
    fn visit_file(&mut self, node: &'ast syn::File) {
        self.parse_debugger_visualizer(&node.attrs);
        visit::visit_file(self, node);
    }

    fn visit_item(&mut self, node: &'ast syn::Item) {
        let attrs = match node {
            syn::Item::Const(node) => &*node.attrs,
            syn::Item::Enum(node) => &*node.attrs,
            syn::Item::ExternCrate(node) => &*node.attrs,
            syn::Item::Fn(node) => &*node.attrs,
            syn::Item::ForeignMod(node) => &*node.attrs,
            syn::Item::Impl(node) => &*node.attrs,
            syn::Item::Macro(node) => &*node.attrs,
            syn::Item::Mod(node) => &*node.attrs,
            syn::Item::Static(node) => &*node.attrs,
            syn::Item::Struct(node) => &*node.attrs,
            syn::Item::Trait(node) => &*node.attrs,
            syn::Item::TraitAlias(node) => &*node.attrs,
            syn::Item::Type(node) => &*node.attrs,
            syn::Item::Union(node) => &*node.attrs,
            syn::Item::Use(node) => &*node.attrs,
            syn::Item::Verbatim(_) | _ => &[],
        };
        if !cfg_test(attrs) {
            visit::visit_item(self, node);
        }
    }

    fn visit_expr(&mut self, node: &'ast syn::Expr) {
        let attrs = match node {
            syn::Expr::Array(node) => &*node.attrs,
            syn::Expr::Assign(node) => &*node.attrs,
            syn::Expr::Async(node) => &*node.attrs,
            syn::Expr::Await(node) => &*node.attrs,
            syn::Expr::Binary(node) => &*node.attrs,
            syn::Expr::Block(node) => &*node.attrs,
            syn::Expr::Break(node) => &*node.attrs,
            syn::Expr::Call(node) => &*node.attrs,
            syn::Expr::Cast(node) => &*node.attrs,
            syn::Expr::Closure(node) => &*node.attrs,
            syn::Expr::Const(node) => &*node.attrs,
            syn::Expr::Continue(node) => &*node.attrs,
            syn::Expr::Field(node) => &*node.attrs,
            syn::Expr::ForLoop(node) => &*node.attrs,
            syn::Expr::Group(node) => &*node.attrs,
            syn::Expr::If(node) => &*node.attrs,
            syn::Expr::Index(node) => &*node.attrs,
            syn::Expr::Infer(node) => &*node.attrs,
            syn::Expr::Let(node) => &*node.attrs,
            syn::Expr::Lit(node) => &*node.attrs,
            syn::Expr::Loop(node) => &*node.attrs,
            syn::Expr::Macro(node) => &*node.attrs,
            syn::Expr::Match(node) => &*node.attrs,
            syn::Expr::MethodCall(node) => &*node.attrs,
            syn::Expr::Paren(node) => &*node.attrs,
            syn::Expr::Path(node) => &*node.attrs,
            syn::Expr::Range(node) => &*node.attrs,
            syn::Expr::RawAddr(node) => &*node.attrs,
            syn::Expr::Reference(node) => &*node.attrs,
            syn::Expr::Repeat(node) => &*node.attrs,
            syn::Expr::Return(node) => &*node.attrs,
            syn::Expr::Struct(node) => &*node.attrs,
            syn::Expr::Try(node) => &*node.attrs,
            syn::Expr::TryBlock(node) => &*node.attrs,
            syn::Expr::Tuple(node) => &*node.attrs,
            syn::Expr::Unary(node) => &*node.attrs,
            syn::Expr::Unsafe(node) => &*node.attrs,
            syn::Expr::While(node) => &*node.attrs,
            syn::Expr::Yield(node) => &*node.attrs,
            syn::Expr::Verbatim(_) | _ => &[],
        };
        if !cfg_test(attrs) {
            visit::visit_expr(self, node);
        }
    }

    fn visit_stmt_macro(&mut self, node: &'ast syn::StmtMacro) {
        if !cfg_test(&node.attrs) {
            visit::visit_stmt_macro(self, node);
        }
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        if !cfg_test(&node.attrs) {
            visit::visit_local(self, node);
        }
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        // Look for #[test] and #[bench].
        for attr in &node.attrs {
            if let syn::Meta::Path(path) = &attr.meta
                && (path.is_ident("test") || path.is_ident("bench"))
            {
                return;
            }
        }
        visit::visit_item_fn(self, node);
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        self.parse_debugger_visualizer(&node.attrs);

        let mut path_attrs = Vec::new();
        let mut has_unconditional_path_attr = false; // #[path = ...]
        let mut has_conditional_path_attr = false; // #[cfg_attr(..., path = ...)]
        for attr in &node.attrs {
            if let syn::Meta::NameValue(meta) = &attr.meta
                && meta.path.is_ident("path")
                && let syn::Expr::Lit(expr) = &meta.value
                && let syn::Lit::Str(lit_str) = &expr.lit
            {
                path_attrs.push(lit_str.value());
                has_unconditional_path_attr = true;
            } else if let syn::Meta::List(meta) = &attr.meta
                && meta.path.is_ident("cfg_attr")
            {
                let mut first = true;
                let _ = meta.parse_nested_meta(|nested| {
                    if first {
                        // Ignore first element, which is the cfg expression.
                        ignore_nested_meta(nested.input)?;
                        first = false;
                    } else {
                        // Parse rest, which are attributes.
                        if nested.path.is_ident("path") {
                            let _: Token![=] = nested.input.parse()?;
                            let lit_str: syn::LitStr = nested.input.parse()?;
                            path_attrs.push(lit_str.value());
                            has_conditional_path_attr = true;
                        } else {
                            ignore_nested_meta(nested.input)?;
                        }
                    }
                    Ok(())
                });
            }
        }

        if node.content.is_some() {
            //
            // mod foo { ... }
            //
            if path_attrs.is_empty() {
                self.mod_ancestors.push(node.ident.unraw().to_string());
                visit::visit_item_mod(self, node);
                self.mod_ancestors.pop();
            }

            //
            // #[path = "..."]
            // mod foo { ... }
            //
            for path_attr in path_attrs {
                self.mod_ancestors.push(path_attr);
                visit::visit_item_mod(self, node);
                self.mod_ancestors.pop();
            }
        } else {
            //
            // mod foo;
            //
            if !has_unconditional_path_attr {
                let default_path = {
                    let mut p = self.mod_parent_dir();
                    p.extend(self.mod_ancestors.iter());
                    p.push(format!("{}.rs", node.ident.unraw()));
                    p
                };

                if !self.parse_and_visit_source_file(&default_path, ModRs::Auto) {
                    let secondary_path = {
                        let mut p = self.mod_parent_dir();
                        p.extend(self.mod_ancestors.iter());
                        p.push(node.ident.unraw().to_string());
                        p.push("mod.rs");
                        p
                    };

                    if !self.parse_and_visit_source_file(&secondary_path, ModRs::Auto) {
                        if has_conditional_path_attr {
                            // Suppress the error. We cannot know whether at
                            // least one conditional path attr always applies.
                        } else {
                            self.push_error(ErrorKind::ModuleNotFound {
                                default_path,
                                secondary_path: Some(secondary_path),
                            });
                        }
                    }
                }
            }

            //
            // #[path = "..."]
            // mod foo;
            //
            for path_attr in path_attrs {
                let source_path = {
                    let mut p = if self.mod_ancestors.is_empty() {
                        parent_dir(self.current).to_owned()
                    } else {
                        self.mod_parent_dir()
                    };
                    p.extend(self.mod_ancestors.iter());
                    normalized_extend_path(&mut p, path_attr);
                    p
                };

                // Files loaded via `#[path = ...]` are treated as "mod-rs" files.
                if !self.parse_and_visit_source_file(&source_path, ModRs::Yes) {
                    self.push_error(ErrorKind::ModuleNotFound {
                        default_path: source_path,
                        secondary_path: None,
                    });
                }
            }
        };
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        let macro_ident = node.path.segments.last().unwrap().ident.to_string();

        // TODO: Consider parsing the `include` for more modules.
        match macro_ident.as_str() {
            "include_str" | "include_bytes" | "include" => {
                match node.parse_body::<syn::LitStr>() {
                    Ok(path) => {
                        if let Err(err) = self.collect_relative_path(&path) {
                            self.push_error(err);
                        }
                    }
                    Err(_err) => {
                        // Ignore error. This happens for nonstring-literal include:
                        // `include!(concat!(env!("OUT_DIR"), "/generated.rs"));`
                    }
                };
            }
            "cfg_if" => match node.parse_body::<CfgIf>() {
                Ok(cfg_if) => {
                    self.visit_cfg_if(&cfg_if);
                }
                Err(err) => {
                    self.push_error(ErrorKind::ParserError {
                        line: err.span().start().line,
                        source_path: self.current.to_owned(),
                        source: err,
                    });
                }
            },
            _ => {}
        };
    }

    fn visit_meta_name_value(&mut self, node: &'ast syn::MetaNameValue) {
        if node.path.is_ident("doc")
            && let syn::Expr::Macro(expr) = &node.value
            && expr.mac.path.segments.last().unwrap().ident == "include_str"
            && let Ok(lit) = expr.mac.parse_body::<syn::LitStr>()
        {
            if let Err(_err) = self.collect_relative_path(&lit) {
                // Ignore error. Crates sometimes have:
                //
                //     #[doc = include_str!("../../README.md")]
                //     #[cfg(doctest)]
                //     pub struct ReadmeDoctests;
                //
                // Within a doc attribute, the included file is practically
                // always a Markdown file not a Rust file, so reporting an error
                // here and switching sources to a fallback of "**/*.rs" would
                // not be any more useful.
            }
        }
    }
}

#[derive(Debug)]
enum CfgExpr {
    Block(syn::Block),
    If(CfgIf),
}

#[derive(Debug)]
struct CfgIf {
    then_branch: syn::Block,
    else_branch: Option<Box<CfgExpr>>,
}

impl syn::parse::Parse for CfgIf {
    fn parse(input: syn::parse::ParseStream) -> Result<Self, syn::Error> {
        let _: Token![if] = input.parse()?;

        // Exactly one attribute.
        let _: Token![#] = input.parse()?;
        let meta;
        syn::bracketed!(meta in input);
        let _: syn::Meta = meta.parse()?;

        Ok(CfgIf {
            then_branch: input.parse()?,
            else_branch: {
                if input.parse::<Option<Token![else]>>()?.is_some() {
                    Some(Box::new(
                        input
                            .parse::<syn::Block>()
                            .map(CfgExpr::Block)
                            .or_else(|_| input.parse::<CfgIf>().map(CfgExpr::If))?,
                    ))
                } else {
                    None
                }
            },
        })
    }
}

fn ignore_nested_meta(input: syn::parse::ParseStream) -> syn::Result<()> {
    if input.parse::<Option<Token![=]>>()?.is_some() {
        let _: syn::Lit = input.parse()?;
    } else if input.peek(syn::token::Paren) {
        let content;
        syn::parenthesized!(content in input);
        let _: TokenStream = content.parse()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    use super::Error;
    use super::crate_srcfiles;

    macro_rules! cargo_manifest_dir {
        ($(
            $path:expr => { $($content:tt)* }
        )+) => {{
            let dir = tempfile::tempdir().unwrap();
            $(
                let file = dir.path().join($path);
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, stringify!($($content)*)).unwrap();
            )+
            dir
        }};
    }

    #[track_caller]
    fn assert_srcfiles(dir: &Path, expected: &[&str]) {
        let res = crate_srcfiles(dir.join("src").join("lib.rs"));

        assert_eq!(
            res.files
                .iter()
                .map(|path| path.strip_prefix(dir).unwrap())
                .collect::<BTreeSet<_>>(),
            expected.iter().map(Path::new).collect::<BTreeSet<_>>(),
        );

        if !res.errors.is_empty() {
            panic!(
                "crate_srcfiles errors: {:#?}",
                res.errors.iter().map(Error::to_string).collect::<Vec<_>>(),
            );
        }
    }

    #[test]
    fn test_mod() {
        let dir = cargo_manifest_dir! {
            // Naming:
            // p = module declaration has path attribute ("Path")
            // c = no path attribute ("Conventional" location)
            // d = module located in subdirectory with mod.rs ("Directory")
            // n = no subdirectory ("Neighbor")
            // i = in-line module with braced contents in the same file ("Inline")
            // ABCD = depth
            "src/lib.rs" => {
                mod A_cn;
                mod A_cd;
                mod A_ci {
                    mod B_cn;
                    mod B_cd;
                    mod B_ci {
                        mod C_cn;
                    }

                    #[path = "bp.rs"]
                    mod B_p;

                    const _: &str = include_str!("str3.txt");
                }

                #[path = "api"]
                mod A_pi {
                    #[path = "bp.rs"]
                    mod B_p;
                }

                const _: &str = include_str!("../str1.txt");
                const _: &str = include_str!("str2.txt");
                const _: &[u8] = include_bytes!("subdir/bytes1.txt");

                include!(concat!(env!("OUT_DIR"), "/generated.rs"));
            }

            "src/A_cn.rs" => {
                mod B_cn;
                mod B_cd;
                mod B_ci {
                    mod r#C_cn;
                    mod r#C_cd;
                    mod r#C_ci {
                        mod D_cn;
                    }
                }

                #[path = "bpi"]
                mod B_pi {
                    // FIXME: this incorrectly uses `src/A_cn/bpi/C_cn.rs`.
                    // Should be `src/bpi/C_cn.rs`.
                    mod C_cn;
                }
            }

            "src/api/bp.rs" => {
                // Because bp.rs was loaded via `path`, it's considered a
                // "mod-rs" file. So dp.rs is loaded relative to `src/api/C_ci/`.
                // If this had not been a "mod-rs" file, dp.rs would be loaded
                // from `src/api/bp/C_ci/`.
                mod C_ci {
                    #[path = "dp.rs"]
                    mod D_p;
                }
                #[path = "../cp.rs"]
                mod C_p;
            }

            "src/A_ci/bp.rs" => {
                mod C_ci {
                    #[path = "dp.rs"]
                    mod D_p;
                }
            }

            "src/A_cn/B_cn.rs" => {}
            "src/A_cn/B_cd/mod.rs" => {}
            "src/A_cn/B_ci/C_cn.rs" => {}
            "src/A_cn/B_ci/C_cd/mod.rs" => {}
            "src/A_cn/B_ci/C_ci/D_cn.rs" => {}
            "src/A_cn/bpi/C_cn.rs" => {}
            "src/A_cd/mod.rs" => {}
            "src/A_ci/B_cn.rs" => {}
            "src/A_ci/B_cd/mod.rs" => {}
            "src/A_ci/B_ci/C_cn.rs" => {}
            "src/A_ci/C_ci/dp.rs" => {}
            "src/api/C_ci/dp.rs" => {}
            "src/cp.rs" => {}
            "str1.txt" => {}
            "src/str2.txt" => {}
            "src/str3.txt" => {}
            "src/subdir/bytes1.txt" => {}
        };

        assert_srcfiles(
            dir.path(),
            &[
                "src/lib.rs",
                "src/A_cn.rs",
                "src/A_cn/B_cn.rs",
                "src/A_cn/B_cd/mod.rs",
                "src/A_cn/B_ci/C_cn.rs",
                "src/A_cn/B_ci/C_cd/mod.rs",
                "src/A_cn/B_ci/C_ci/D_cn.rs",
                "src/A_cn/bpi/C_cn.rs",
                "src/A_cd/mod.rs",
                "src/A_ci/B_cn.rs",
                "src/A_ci/B_cd/mod.rs",
                "src/A_ci/B_ci/C_cn.rs",
                "src/A_ci/C_ci/dp.rs",
                "src/A_ci/bp.rs",
                "src/api/bp.rs",
                "src/api/C_ci/dp.rs",
                "src/cp.rs",
                "src/str2.txt",
                "src/str3.txt",
                "src/subdir/bytes1.txt",
                "str1.txt",
            ],
        );
    }

    #[test]
    fn test_cfg_if() {
        let dir = cargo_manifest_dir! {
            "src/lib.rs" => {
                cfg_if::cfg_if! {
                    if #[cfg(target_os = "linux")] {
                        mod linux;
                    } else if #[cfg(windows)] {
                        mod windows;
                    } else {
                        mod unknown;
                    }
                }
            }
            "src/linux.rs" => {}
            "src/windows.rs" => {}
            "src/unknown.rs" => {}
        };

        assert_srcfiles(
            dir.path(),
            &[
                "src/lib.rs",
                "src/linux.rs",
                "src/windows.rs",
                "src/unknown.rs",
            ],
        );
    }

    #[test]
    fn test_doc_include() {
        let dir = cargo_manifest_dir! {
            "src/lib.rs" => {
                #[doc = include_str!("../README.md")]
                const _: () = ();
            }
            "README.md" => {}
        };

        assert_srcfiles(dir.path(), &["src/lib.rs", "README.md"]);
    }

    #[test]
    fn test_debugger_visualizer() {
        let dir = cargo_manifest_dir! {
            "src/lib.rs" => {
                #[cfg(windows)]
                mod windows;
                #[cfg(unix)]
                #[debugger_visualizer(gdb_script_file = "unix.py")]
                mod unix;
            }
            "src/windows.rs" => {
                #![debugger_visualizer(natvis_file = "windows.natvis")]
            }
            "src/unix.rs" => {}
            "src/windows.natvis" => {}
            "src/unix.py" => {}
            "src/unused.natvis" => {}
            "src/unused.py" => {}
        };

        assert_srcfiles(
            dir.path(),
            &[
                "src/lib.rs",
                "src/unix.py",
                "src/unix.rs",
                "src/windows.natvis",
                "src/windows.rs",
            ],
        );
    }
}
