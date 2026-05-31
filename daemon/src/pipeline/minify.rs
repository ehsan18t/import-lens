use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_mangler::Mangler;
use oxc_minifier::{Minifier, MinifierOptions};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{TransformOptions, Transformer};
use std::path::Path;

pub fn minify_source(source: &str, is_cjs: bool) -> Result<String, String> {
    let allocator = Allocator::default();
    let source_type = if is_cjs {
        SourceType::cjs()
    } else {
        SourceType::mjs()
    };
    let parsed = Parser::new(&allocator, source, source_type).parse();

    if parsed.panicked || !parsed.errors.is_empty() {
        return Err(format!(
            "failed to parse bundled source before minification: {}",
            parsed
                .errors
                .iter()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let mut program = parsed.program;
    let semantic = SemanticBuilder::new()
        .with_check_syntax_error(true)
        .build(&program);
    if !semantic.errors.is_empty() {
        return Err(format!(
            "semantic validation failed before minification: {}",
            semantic
                .errors
                .iter()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let transform = Transformer::new(
        &allocator,
        Path::new("import-lens-bundle.js"),
        &TransformOptions::default(),
    )
    .build_with_scoping(semantic.semantic.into_scoping(), &mut program);
    if !transform.errors.is_empty() {
        return Err(format!(
            "transform failed before minification: {}",
            transform
                .errors
                .iter()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let _minified = Minifier::new(MinifierOptions::default()).minify(&allocator, &mut program);
    let mangled = Mangler::new().build(&program);
    let generated = Codegen::new()
        .with_options(CodegenOptions::minify())
        .with_scoping(Some(mangled.scoping))
        .with_private_member_mappings(Some(mangled.class_private_mappings))
        .build(&program);

    Ok(generated.code)
}
