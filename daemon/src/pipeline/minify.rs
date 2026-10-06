use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_minifier::{Minifier, MinifierOptions};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;

/// Minifies a Rolldown output chunk, which is always an ES module.
pub fn minify_source(source: &str) -> Result<String, String> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::mjs()).parse();

    if parsed.fatal_error || parsed.diagnostics.has_errors() {
        return Err(format!(
            "failed to parse linked source before minification: {}",
            parsed
                .diagnostics
                .errors()
                // §5.1: this string reaches the user. `Display` is the dependency's
                // stable message; `Debug` leaks its internal representation and
                // changes shape on any compiler-stack bump.
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let mut program = parsed.program;
    let semantic = SemanticBuilder::new_compiler().build(&program);
    if semantic.diagnostics.has_errors() {
        return Err(format!(
            "semantic validation failed before minification: {}",
            semantic
                .diagnostics
                .errors()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let minified = Minifier::new(MinifierOptions::default()).minify(&allocator, &mut program);
    let generated = Codegen::new()
        .with_options(CodegenOptions::minify())
        .with_scoping(minified.scoping)
        .with_private_member_mappings(minified.class_private_mappings)
        .build(&program);

    Ok(generated.code)
}
