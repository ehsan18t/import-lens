use import_lens_daemon::cache::key::cache_key_for_import;
use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest};

#[test]
fn cache_key_sorts_named_exports() {
    let request = ImportRequest {
        specifier: "lodash-es".to_owned(),
        package_name: "lodash-es".to_owned(),
        version: "4.17.21".to_owned(),
        named: vec!["throttle".to_owned(), "debounce".to_owned()],
        import_kind: ImportKind::Named,
    };

    assert_eq!(
        cache_key_for_import(&request),
        "lodash-es@4.17.21::debounce,throttle"
    );
}

#[test]
fn cache_key_uses_sentinels_for_default_namespace_and_dynamic() {
    let default_request = ImportRequest {
        specifier: "react".to_owned(),
        package_name: "react".to_owned(),
        version: "18.3.1".to_owned(),
        named: vec![],
        import_kind: ImportKind::Default,
    };
    let namespace_request = ImportRequest {
        specifier: "lodash-es".to_owned(),
        package_name: "lodash-es".to_owned(),
        version: "4.17.21".to_owned(),
        named: vec![],
        import_kind: ImportKind::Namespace,
    };
    let dynamic_request = ImportRequest {
        specifier: "date-fns".to_owned(),
        package_name: "date-fns".to_owned(),
        version: "3.6.0".to_owned(),
        named: vec![],
        import_kind: ImportKind::Dynamic,
    };

    assert_eq!(
        cache_key_for_import(&default_request),
        "react@18.3.1::default"
    );
    assert_eq!(
        cache_key_for_import(&namespace_request),
        "lodash-es@4.17.21::*"
    );
    assert_eq!(
        cache_key_for_import(&dynamic_request),
        "date-fns@3.6.0::dynamic"
    );
}

#[test]
fn cache_key_preserves_subpath_specifier_component() {
    let request = ImportRequest {
        specifier: "date-fns/format".to_owned(),
        package_name: "date-fns".to_owned(),
        version: "3.6.0".to_owned(),
        named: vec![],
        import_kind: ImportKind::Default,
    };

    assert_eq!(
        cache_key_for_import(&request),
        "date-fns/format@3.6.0::default"
    );
}
