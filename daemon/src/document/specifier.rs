use crate::pipeline::graph::is_node_builtin_specifier;

pub fn get_package_name(specifier: &str) -> String {
    if specifier.starts_with('@') {
        let mut parts = specifier.split('/');
        let scope = parts.next();
        let name = parts.next();
        return match (scope, name) {
            (Some(scope), Some(name)) => format!("{scope}/{name}"),
            _ => specifier.to_owned(),
        };
    }

    specifier.split('/').next().unwrap_or(specifier).to_owned()
}

pub fn is_runtime_package_specifier(specifier: &str) -> bool {
    !is_relative_specifier(specifier)
        && !is_node_builtin_specifier(specifier)
        && !is_url_like_specifier(specifier)
        && !is_framework_virtual_specifier(specifier)
        && !is_host_provided_module(specifier)
}

fn is_relative_specifier(specifier: &str) -> bool {
    specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.starts_with('/')
        || specifier.starts_with("\\\\")
        || specifier.starts_with(".\\")
        || specifier.starts_with("..\\")
        || is_windows_absolute_path(specifier)
}

fn is_windows_absolute_path(specifier: &str) -> bool {
    let bytes = specifier.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn is_url_like_specifier(specifier: &str) -> bool {
    let Some(colon_index) = specifier.find(':') else {
        return false;
    };

    let scheme = &specifier[..colon_index];
    let mut chars = scheme.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    first.is_ascii_alphabetic()
        && chars.all(|char| char.is_ascii_alphanumeric() || matches!(char, '+' | '.' | '-'))
}

fn is_framework_virtual_specifier(specifier: &str) -> bool {
    specifier.starts_with("astro:")
        || specifier.starts_with("virtual:")
        || specifier.starts_with('$')
        || specifier.starts_with('#')
        || specifier.starts_with("@/")
        || specifier.starts_with("~/")
}

fn is_host_provided_module(specifier: &str) -> bool {
    matches!(specifier, "vscode" | "electron") || specifier.starts_with("bun:")
}
