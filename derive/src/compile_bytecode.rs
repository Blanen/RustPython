//! Parsing and processing for this form:
//! ```ignore
//! py_compile_input!(
//!     // either:
//!     source = "python_source_code",
//!     // or
//!     file = "file/path/relative/to/$CARGO_MANIFEST_DIR",
//!
//!     // the mode to compile the code in
//!     mode = "exec", // or "eval" or "single"
//!     // the path put into the CodeObject, defaults to "frozen"
//!     module_name = "frozen",
//! )
//! ```

use crate::{extract_spans, DiagResult, Diagnostic};
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use rustpython_bytecode::bytecode::{CodeObject, FrozenModule};
use rustpython_compiler::compile;
use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use syn::parse::{Parse, ParseStream, Result as ParseResult};
use syn::{self, parse2, Lit, LitByteStr, LitStr, Meta, Token};

enum CompilationSourceKind {
    File(PathBuf),
    SourceCode(String),
    Dir(PathBuf),
}

struct CompilationSource {
    kind: CompilationSourceKind,
    span: (Span, Span),
}

impl CompilationSource {
    fn compile_string(
        &self,
        source: &str,
        mode: compile::Mode,
        module_name: String,
    ) -> DiagResult<CodeObject> {
        compile::compile(source, mode, module_name, 0)
            .map_err(|err| Diagnostic::spans_error(self.span, format!("Compile error: {}", err)))
    }

    fn compile(
        &self,
        mode: compile::Mode,
        module_name: String,
    ) -> DiagResult<HashMap<String, FrozenModule>> {
        let map = match &self.kind {
            CompilationSourceKind::File(rel_path) => {
                let mut path = PathBuf::from(
                    env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not present"),
                );
                path.push(rel_path);
                if path.is_dir() {
                    return self.compile_dir(
                        &path,
                        path.to_string_lossy().into(),
                        compile::Mode::Exec,
                    );
                }
                let source = fs::read_to_string(&path).map_err(|err| {
                    Diagnostic::spans_error(
                        self.span,
                        format!("Error reading file {:?}: {}", path, err),
                    )
                })?;
                hashmap! {
                    module_name.clone() => FrozenModule {
                        code: self.compile_string(&source, mode, module_name.clone())?,
                        package: false,
                    },
                }
            }
            CompilationSourceKind::SourceCode(code) => {
                hashmap! {
                    module_name.clone() => FrozenModule {
                        code: self.compile_string(code, mode, module_name.clone())?,
                        package: false,
                    },
                }
            }
            CompilationSourceKind::Dir(rel_path) => {
                let mut path = PathBuf::from(
                    env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not present"),
                );
                path.push(rel_path);
                self.compile_dir(&path, String::new(), mode)?
            }
        };
        Ok(map)
    }

    fn compile_dir(
        &self,
        path: &Path,
        parent: String,
        mode: compile::Mode,
    ) -> DiagResult<HashMap<String, FrozenModule>> {
        let mut code_map = HashMap::new();

        let paths = fs::read_dir(&path).map_err(|err| {
            Diagnostic::spans_error(self.span, format!("Error listing dir {:?}: {}", path, err))
        })?;

        for entry in paths {
            let entry: fs::DirEntry = entry.map_err(|err| {
                Diagnostic::spans_error(self.span, format!("Failed to list file: {}", err))
            })?;
            let path = entry.path();

            let module_name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| {
                    Diagnostic::spans_error(
                        self.span,
                        format!("Couldn't get module name from path {:?}", path),
                    )
                })?
                .to_string();

            let filepath: std::borrow::Cow<Path> = match path.extension().and_then(OsStr::to_str) {
                Some("py") => path.into(),
                None if path.is_dir() => path.into(),
                Some("pylink") => {
                    let s = fs::read_to_string(&path).map_err(|err| {
                        Diagnostic::spans_error(
                            self.span,
                            format!("Couldn't read pylink file {:?}: {}", path, err),
                        )
                    })?;
                    path.parent().unwrap().join(s.trim()).into()
                }
                _ => continue,
            };

            if filepath.is_dir() {
                code_map.extend(self.compile_dir(
                    &filepath,
                    format!("{}{}", parent, module_name),
                    mode,
                )?);
            } else {
                let source = fs::read_to_string(&filepath).map_err(|err| {
                    Diagnostic::spans_error(
                        self.span,
                        format!("Error reading file {:?}: {}", filepath, err),
                    )
                })?;
                let is_init = module_name == "__init__";
                let module_name = if is_init {
                    parent.clone()
                } else if parent.is_empty() {
                    module_name
                } else {
                    format!("{}.{}", parent, module_name)
                };
                code_map.insert(
                    module_name.clone(),
                    FrozenModule {
                        code: self.compile_string(&source, mode, module_name)?,
                        package: is_init,
                    },
                );
            }
        }
        Ok(code_map)
    }
}

/// This is essentially just a comma-separated list of Meta nodes, aka the inside of a MetaList.
struct PyCompileInput {
    span: Span,
    metas: Vec<Meta>,
}

impl PyCompileInput {
    fn compile(&self) -> DiagResult<HashMap<String, FrozenModule>> {
        let mut module_name = None;
        let mut mode = None;
        let mut source: Option<CompilationSource> = None;

        fn assert_source_empty(source: &Option<CompilationSource>) -> DiagResult<()> {
            if let Some(source) = source {
                Err(Diagnostic::spans_error(
                    source.span,
                    "Cannot have more than one source",
                ))
            } else {
                Ok(())
            }
        }

        for meta in &self.metas {
            if let Meta::NameValue(name_value) = meta {
                if name_value.ident == "mode" {
                    match &name_value.lit {
                        Lit::Str(s) => match s.value().parse() {
                            Ok(mode_val) => mode = Some(mode_val),
                            Err(e) => bail_span!(s, "{}", e),
                        },
                        _ => bail_span!(name_value.lit, "mode must be a string"),
                    }
                } else if name_value.ident == "module_name" {
                    module_name = Some(match &name_value.lit {
                        Lit::Str(s) => s.value(),
                        _ => bail_span!(name_value.lit, "module_name must be string"),
                    })
                } else if name_value.ident == "source" {
                    assert_source_empty(&source)?;
                    let code = match &name_value.lit {
                        Lit::Str(s) => s.value(),
                        _ => bail_span!(name_value.lit, "source must be a string"),
                    };
                    source = Some(CompilationSource {
                        kind: CompilationSourceKind::SourceCode(code),
                        span: extract_spans(&name_value).unwrap(),
                    });
                } else if name_value.ident == "file" {
                    assert_source_empty(&source)?;
                    let path = match &name_value.lit {
                        Lit::Str(s) => PathBuf::from(s.value()),
                        _ => bail_span!(name_value.lit, "source must be a string"),
                    };
                    source = Some(CompilationSource {
                        kind: CompilationSourceKind::File(path),
                        span: extract_spans(&name_value).unwrap(),
                    });
                } else if name_value.ident == "dir" {
                    assert_source_empty(&source)?;
                    let path = match &name_value.lit {
                        Lit::Str(s) => PathBuf::from(s.value()),
                        _ => bail_span!(name_value.lit, "source must be a string"),
                    };
                    source = Some(CompilationSource {
                        kind: CompilationSourceKind::Dir(path),
                        span: extract_spans(&name_value).unwrap(),
                    });
                }
            }
        }

        source
            .ok_or_else(|| {
                Diagnostic::span_error(
                    self.span,
                    "Must have either file or source in py_compile_bytecode!()",
                )
            })?
            .compile(
                mode.unwrap_or(compile::Mode::Exec),
                module_name.unwrap_or_else(|| "frozen".to_string()),
            )
    }
}

impl Parse for PyCompileInput {
    fn parse(input: ParseStream) -> ParseResult<Self> {
        let span = input.cursor().span();
        let metas = input
            .parse_terminated::<Meta, Token![,]>(Meta::parse)?
            .into_iter()
            .collect();
        Ok(PyCompileInput { span, metas })
    }
}

pub fn impl_py_compile_bytecode(input: TokenStream2) -> DiagResult<TokenStream2> {
    let input: PyCompileInput = parse2(input)?;

    let code_map = input.compile()?;

    let modules = code_map
        .into_iter()
        .map(|(module_name, FrozenModule { code, package })| {
            let module_name = LitStr::new(&module_name, Span::call_site());
            let bytes = code.to_bytes();
            let bytes = LitByteStr::new(&bytes, Span::call_site());
            quote! {
                #module_name.into() => ::rustpython_vm::bytecode::FrozenModule {
                    code: ::rustpython_vm::bytecode::CodeObject::from_bytes(
                        #bytes
                    ).expect("Deserializing CodeObject failed"),
                    package: #package,
                }
            }
        });

    let output = quote! {
        ({
            use ::rustpython_vm::__exports::hashmap;
            hashmap! {
                #(#modules),*
            }
        })
    };

    Ok(output)
}
