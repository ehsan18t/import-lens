use oxc_allocator::Allocator;
use oxc_ast::ast::{Expression, ModuleExportName, Program, Statement};
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_minifier::{Minifier, MinifierOptions};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{TransformOptions, Transformer};
use std::path::Path;

pub fn minify_source(source: &str, is_cjs: bool) -> Result<String, String> {
    minify_source_inner(source, is_cjs, false)
}

pub fn minify_source_with_markers(source: &str, is_cjs: bool) -> Result<String, String> {
    minify_source_inner(source, is_cjs, true)
}

pub fn validate_source(source: &str, is_cjs: bool) -> Result<(), String> {
    let allocator = Allocator::default();
    let source_type = if is_cjs {
        SourceType::cjs()
    } else {
        SourceType::mjs()
    };
    let parsed = Parser::new(&allocator, source, source_type).parse();

    if parsed.panicked || parsed.diagnostics.has_errors() {
        return Err(format!(
            "failed to parse generated source: {}",
            parsed
                .diagnostics
                .errors()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let semantic = SemanticBuilder::new()
        .with_check_syntax_error(true)
        .build(&parsed.program);
    if semantic.diagnostics.has_errors() {
        return Err(format!(
            "semantic validation failed for generated source: {}",
            semantic
                .diagnostics
                .errors()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    Ok(())
}

fn minify_source_inner(
    source: &str,
    is_cjs: bool,
    remove_import_lens_markers: bool,
) -> Result<String, String> {
    let allocator = Allocator::default();
    let source_type = if is_cjs {
        SourceType::cjs()
    } else {
        SourceType::mjs()
    };
    let parsed = Parser::new(&allocator, source, source_type).parse();

    if parsed.panicked || parsed.diagnostics.has_errors() {
        return Err(format!(
            "failed to parse bundled source before minification: {}",
            parsed
                .diagnostics
                .errors()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let mut program = parsed.program;
    let semantic = SemanticBuilder::new()
        .with_check_syntax_error(true)
        .build(&program);
    if semantic.diagnostics.has_errors() {
        return Err(format!(
            "semantic validation failed before minification: {}",
            semantic
                .diagnostics
                .errors()
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
    if transform.diagnostics.has_errors() {
        return Err(format!(
            "transform failed before minification: {}",
            transform
                .diagnostics
                .errors()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let minified = Minifier::new(MinifierOptions::default()).minify(&allocator, &mut program);
    if remove_import_lens_markers {
        remove_import_lens_marker_statements(&mut program);
    }
    let generated = Codegen::new()
        .with_options(CodegenOptions::minify())
        .with_scoping(minified.scoping)
        .with_private_member_mappings(minified.class_private_mappings)
        .build(&program);

    Ok(generated.code)
}

fn remove_import_lens_marker_statements(program: &mut Program<'_>) {
    program
        .body
        .retain(|statement| !is_import_lens_marker_statement(statement));
}

fn is_import_lens_marker_statement(statement: &Statement<'_>) -> bool {
    if is_import_lens_marker_export_statement(statement) {
        return true;
    }

    let Statement::ExpressionStatement(statement) = statement else {
        return false;
    };
    let Expression::CallExpression(call) = &statement.expression else {
        return false;
    };
    let Expression::Identifier(callee) = &call.callee else {
        return false;
    };

    callee.name.as_str() == "__importLensUse"
}

fn is_import_lens_marker_export_statement(statement: &Statement<'_>) -> bool {
    let Statement::ExportNamedDeclaration(export) = statement else {
        return false;
    };

    export.declaration.is_none()
        && export.source.is_none()
        && !export.specifiers.is_empty()
        && export.specifiers.iter().all(|specifier| {
            module_export_name(&specifier.exported)
                .is_some_and(|name| name.starts_with("__importLensUse_"))
        })
}

fn module_export_name<'a>(name: &'a ModuleExportName<'a>) -> Option<&'a str> {
    match name {
        ModuleExportName::IdentifierName(name) => Some(name.name.as_str()),
        ModuleExportName::IdentifierReference(name) => Some(name.name.as_str()),
        ModuleExportName::StringLiteral(name) => Some(name.value.as_str()),
        _ => None,
    }
}
