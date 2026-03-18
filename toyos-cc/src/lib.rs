#[macro_use]
mod verbose;
mod ast;
mod codegen;
mod emit;
mod lex;
mod parse;
mod preprocess;
mod types;

use std::path::{Path, PathBuf};

/// Options for compiling a C source file to an object file.
pub struct CompileOptions {
    pub include_paths: Vec<PathBuf>,
    pub defines: Vec<(String, String)>,
    pub target: Option<String>,
    pub opt_level: u8,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            include_paths: Vec::new(),
            defines: Vec::new(),
            target: None,
            opt_level: 0,
        }
    }
}

/// Compile a C source string to object file bytes.
///
/// `filename` is used for error messages and `__FILE__`.
pub fn compile(source: &str, filename: &str, options: &CompileOptions) -> Vec<u8> {
    let mut pp = preprocess::Preprocessor::new(
        options.include_paths.clone(),
        options.defines.clone(),
        options.target.as_deref(),
    );
    pp.suppress_line_markers = false;
    let preprocessed = pp.preprocess(source, filename);

    let lexer = lex::Lexer::new(&preprocessed, filename);
    let tokens = lexer.tokenize();

    let parser = parse::Parser::new(tokens);
    let (tu, type_env) = parser.parse();

    let obj_name = Path::new(filename)
        .with_extension("o")
        .to_string_lossy()
        .into_owned();
    let module = emit::create_module(&obj_name, options.target.as_deref(), options.opt_level);
    let mut cg = codegen::Codegen::new(module, type_env);
    cg.compile_unit(&tu);
    cg.define_variadic_stubs();

    emit::finish(cg.module)
}

/// Preprocess a C source string, returning the preprocessed text.
pub fn preprocess_source(
    source: &str,
    filename: &str,
    options: &CompileOptions,
    suppress_line_markers: bool,
) -> String {
    let mut pp = preprocess::Preprocessor::new(
        options.include_paths.clone(),
        options.defines.clone(),
        options.target.as_deref(),
    );
    pp.suppress_line_markers = suppress_line_markers;
    pp.preprocess(source, filename)
}
