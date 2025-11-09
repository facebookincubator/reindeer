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
use proc_macro2 as _; // To autocargo with our features (namely `span-locations`)
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

    fn collect_relative_path(&mut self, path: &syn::LitStr) {
        let source_path = {
            let mut p = parent_dir(self.current).to_owned();
            normalized_extend_path(&mut p, path.value());
            p
        };
        match fs::File::open(&source_path) {
            Ok(_) => {
                self.sources.files.insert(source_path);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.push_error(ErrorKind::IncludeNotFound { source_path });
            }
            Err(err) => {
                self.push_error(ErrorKind::FileError {
                    source_path,
                    source: err,
                });
            }
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
                        self.collect_relative_path(&lit);
                    }
                    Ok(())
                });
            }
        }
    }
}

fn parse_possible_path(attr: &syn::Attribute) -> Option<String> {
    if attr.path().is_ident("path") {
        if let syn::Meta::NameValue(meta) = &attr.meta {
            if let syn::Expr::Lit(expr) = &meta.value {
                if let syn::Lit::Str(lit_str) = &expr.lit {
                    return Some(lit_str.value());
                }
            }
        }
    }
    None
}

fn parent_dir(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new(".."))
}

fn cfg_test(attrs: &[syn::Attribute]) -> bool {
    // Look for #[cfg(test)].
    for attr in attrs {
        let mut is_cfg_test = None;
        if let syn::Meta::List(meta) = &attr.meta
            && meta.path.is_ident("cfg")
            && meta
                .parse_nested_meta(|nested| {
                    *is_cfg_test.get_or_insert(true) &= nested.path.is_ident("test");
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
        // Look for #[test].
        for attr in &node.attrs {
            if let syn::Meta::Path(path) = &attr.meta
                && path.is_ident("test")
            {
                return;
            }
        }
        visit::visit_item_fn(self, node);
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        self.parse_debugger_visualizer(&node.attrs);

        // https://doc.rust-lang.org/reference/items/modules.html
        // rustc only looks at the first `path` attribute.
        // https://github.com/rust-lang/rust/blob/1.69.0/compiler/rustc_expand/src/module.rs
        let first_path_value = node.attrs.iter().find_map(parse_possible_path);

        match (first_path_value, &node.content) {
            //
            // mod foo;
            //
            (None, None) => {
                let default_path = {
                    let mut p = self.mod_parent_dir();
                    p.extend(self.mod_ancestors.iter());
                    p.push(format!("{}.rs", node.ident));
                    p
                };

                if !self.parse_and_visit_source_file(&default_path, ModRs::Auto) {
                    let secondary_path = {
                        let mut p = self.mod_parent_dir();
                        p.extend(self.mod_ancestors.iter());
                        p.push(node.ident.to_string());
                        p.push("mod.rs");
                        p
                    };

                    if !self.parse_and_visit_source_file(&secondary_path, ModRs::Auto) {
                        self.push_error(ErrorKind::ModuleNotFound {
                            default_path,
                            secondary_path: Some(secondary_path),
                        });
                    }
                }
            }

            //
            // #[path = "..."]
            // mod foo;
            //
            (Some(first_path_value), None) => {
                let source_path = {
                    let mut p = if self.mod_ancestors.is_empty() {
                        parent_dir(self.current).to_owned()
                    } else {
                        self.mod_parent_dir()
                    };
                    p.extend(self.mod_ancestors.iter());
                    normalized_extend_path(&mut p, first_path_value);
                    p
                };

                // Files loaded via `#[path = ...]` are treated as "mod-rs"
                // files. Makes sense if you think about it: If it wasn't
                // treated as a "mod-rs" file, then what about be the module
                // dir for a file named `.weird.name.txt`.
                if !self.parse_and_visit_source_file(&source_path, ModRs::Yes) {
                    self.push_error(ErrorKind::ModuleNotFound {
                        default_path: source_path,
                        secondary_path: None,
                    });
                }
            }

            //
            // mod foo { ... }
            //
            (None, Some(_)) => {
                self.mod_ancestors.push(node.ident.to_string());
                syn::visit::visit_item_mod(self, node);
                self.mod_ancestors.pop();
            }

            //
            // #[path = "..."]
            // mod foo { ... }
            //
            (Some(first_path_value), Some(_)) => {
                self.mod_ancestors.push(first_path_value);
                syn::visit::visit_item_mod(self, node);
                self.mod_ancestors.pop();
            }
        };
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        let macro_ident = node.path.segments.last().unwrap().ident.to_string();

        // TODO: Consider parsing the `include` for more modules.
        match macro_ident.as_str() {
            "include_str" | "include_bytes" | "include" => {
                match node.parse_body::<syn::LitStr>() {
                    Ok(path) => self.collect_relative_path(&path),
                    Err(err) => {
                        self.push_error(ErrorKind::ParserError {
                            line: err.span().start().line,
                            source_path: self.current.to_owned(),
                            source: err,
                        });
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
        let _: syn::Token![if] = input.parse()?;

        // Exactly one attribute.
        let _: syn::Token![#] = input.parse()?;
        let meta;
        syn::bracketed!(meta in input);
        let _: syn::Meta = meta.parse()?;

        Ok(CfgIf {
            then_branch: input.parse()?,
            else_branch: {
                if input.parse::<Option<syn::Token![else]>>()?.is_some() {
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
    fn test_mod_without_path_attr() {
        let dir = cargo_manifest_dir! {
            "src/lib.rs" => {
                mod aaa;
                mod bbb;
                mod ccc {
                    mod ddd;
                    mod eee;
                    mod fff {
                        mod ggg;
                    }

                    const _: &str = include_str!("str3.txt");
                }

                const _: &str = include_str!("../str1.txt");
                const _: &str = include_str!("str2.txt");
                const _: &[u8] = include_bytes!("subdir/bytes1.txt");
            }

            "src/aaa.rs" => {
                mod mmm;
                mod nnn;
                mod ooo {
                    mod ppp;
                    mod qqq;
                    mod rrr {
                        mod sss;
                    }
                }
            }
            "src/bbb/mod.rs" => {}
            "src/ccc/ddd.rs" => {}
            "src/ccc/eee/mod.rs" => {}
            "src/ccc/fff/ggg.rs" => {}
            "src/aaa/mmm.rs" => {}
            "src/aaa/nnn/mod.rs" => {}
            "src/aaa/ooo/ppp.rs" => {}
            "src/aaa/ooo/qqq/mod.rs" => {}
            "src/aaa/ooo/rrr/sss.rs" => {}
            "str1.txt" => {}
            "src/str2.txt" => {}
            "src/str3.txt" => {}
            "src/subdir/bytes1.txt" => {}
        };

        assert_srcfiles(
            dir.path(),
            &[
                "src/lib.rs",
                "src/aaa.rs",
                "src/bbb/mod.rs",
                "src/ccc/ddd.rs",
                "src/ccc/eee/mod.rs",
                "src/ccc/fff/ggg.rs",
                "src/aaa/mmm.rs",
                "src/aaa/nnn/mod.rs",
                "src/aaa/ooo/ppp.rs",
                "src/aaa/ooo/qqq/mod.rs",
                "src/aaa/ooo/rrr/sss.rs",
                "src/str2.txt",
                "src/str3.txt",
                "src/subdir/bytes1.txt",
                "str1.txt",
            ],
        );
    }

    #[test]
    fn test_mod_with_path_attr() {
        let dir = cargo_manifest_dir! {
            "src/lib.rs" => {
                #[path = "a"]
                mod aaa {
                    #[path = "b.rs"]
                    mod bbb;
                }

                mod ccc {
                    #[path = "d.rs"]
                    mod ddd;
                }
            }
            "src/a/b.rs" => {
                // Because `b.rs` was loaded via `path`, it's considered a
                // "mod-rs" file. So `n.rs` is loaded relative to `src/a/mmm/`.
                // If this had not been a "mod-rs" file, `n.rs` would be loaded
                // from `src/a/b/mmm/`.
                mod mmm {
                    #[path = "n.rs"]
                    mod nnn;
                }
                #[path = "../ppp.rs"]
                mod ppp;
            }
            "src/a/mmm/n.rs" => {}
            "src/ccc/d.rs" => {
                mod yyy {
                    #[path = "z.rs"]
                    mod zzz;
                }
            }
            "src/ccc/yyy/z.rs" => {}
            "src/ppp.rs" => {}
        };

        assert_srcfiles(
            dir.path(),
            &[
                "src/lib.rs",
                "src/a/b.rs",
                "src/a/mmm/n.rs",
                "src/ccc/d.rs",
                "src/ccc/yyy/z.rs",
                "src/ppp.rs",
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
