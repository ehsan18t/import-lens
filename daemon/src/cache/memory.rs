use crate::ipc::protocol::ImportResult;
use papaya::HashMap;

#[derive(Debug, Default)]
pub struct ImportCache {
    entries: HashMap<String, ImportResult>,
}

impl ImportCache {
    pub fn get(&self, key: &str) -> Option<ImportResult> {
        let entries = self.entries.pin();
        let mut result = entries.get(key)?.clone();
        result.cache_hit = true;
        Some(result)
    }

    pub fn insert(&self, key: String, result: ImportResult) {
        self.entries.pin().insert(key, result);
    }

    pub fn invalidate_package(&self, package_name: &str) {
        let entries = self.entries.pin();
        let root_prefix = format!("{package_name}@");
        let subpath_prefix = format!("{package_name}/");
        let keys = entries
            .iter()
            .filter_map(|(key, _)| {
                (key.starts_with(&root_prefix) || key.starts_with(&subpath_prefix))
                    .then(|| key.clone())
            })
            .collect::<Vec<_>>();

        for key in keys {
            entries.remove(&key);
        }
    }

    pub fn clear(&self) {
        self.entries.pin().clear();
    }
}
