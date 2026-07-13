use crate::ipc::protocol::{
    ConfidenceLevel, DetectedImport, DuplicateImportGroup, DuplicateModuleGroup, ImportResult,
    WorkspaceReportBudgets, WorkspaceReportRow, WorkspaceReportSummary, WorkspaceReportTreemapItem,
};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub struct WorkspaceReportItem {
    pub detected: DetectedImport,
    pub source_file: String,
    pub workspace_root: String,
    pub result: Option<ImportResult>,
    pub warning: Option<String>,
}

/// Rows plus summary counters derived structurally during row construction.
/// The TypeScript `reportModel.ts` derives `conservativeCount` /
/// `budgetViolationCount` by substring-matching the formatted warning text
/// (`"Conservative estimate"` and `/budget exceeded/i`, the latter matching
/// both per-import "Budget exceeded" and per-file "File budget exceeded"
/// warnings). The counters here are computed from the same structured facts
/// that produce those warning strings, so the counts stay in lockstep with
/// the TS semantics without fragile text matching.
pub struct WorkspaceReportRowSet {
    pub rows: Vec<WorkspaceReportRow>,
    pub conservative_count: u64,
    pub budget_violation_count: u64,
}

pub fn build_report_rows(
    items: &[WorkspaceReportItem],
    budgets: &WorkspaceReportBudgets,
) -> WorkspaceReportRowSet {
    let mut rows = items
        .iter()
        .map(|item| row_for_item(item, budgets))
        .collect::<Vec<_>>();
    let conservative_count = items
        .iter()
        .filter(|item| is_conservative_item(item))
        .count() as u64;
    let import_violations = items
        .iter()
        .map(|item| is_import_budget_violation(item, budgets))
        .collect::<Vec<_>>();
    let file_violations = apply_file_budget_warnings(&mut rows, budgets);
    let budget_violation_count = import_violations
        .iter()
        .zip(&file_violations)
        .filter(|(import_violation, file_violation)| **import_violation || **file_violation)
        .count() as u64;
    rows.sort_by_cached_key(|row| {
        (
            Reverse(row.brotli_bytes),
            format!("{}:{}:{}", row.source_file, row.line, row.specifier),
        )
    });

    WorkspaceReportRowSet {
        rows,
        conservative_count,
        budget_violation_count,
    }
}

pub fn build_report_summary(row_set: &WorkspaceReportRowSet) -> WorkspaceReportSummary {
    let rows = &row_set.rows;
    // Only a MEASURED row contributes. An unmeasured one has no bytes to add, and
    // `.unwrap_or_default()` here would silently fold a fabricated zero into the workspace total.
    let total_brotli_bytes = rows.iter().filter_map(|row| row.brotli_bytes).sum::<u64>();
    WorkspaceReportSummary {
        import_count: rows.len() as u64,
        total_brotli_bytes,
        low_confidence_count: rows.iter().filter(|row| row.confidence == "low").count() as u64,
        medium_confidence_count: rows.iter().filter(|row| row.confidence == "medium").count()
            as u64,
        conservative_count: row_set.conservative_count,
        budget_violation_count: row_set.budget_violation_count,
        duplicate_imports: build_duplicate_import_groups(rows),
        shared_modules: build_duplicate_module_groups(rows),
        treemap: build_treemap(rows, total_brotli_bytes),
    }
}

/// Mirrors the condition that pushes "Conservative estimate" into the row
/// warning (TS: `is_cjs || side_effects || truly_treeshakeable === false`).
fn is_conservative_item(item: &WorkspaceReportItem) -> bool {
    item.result
        .as_ref()
        .is_some_and(|result| result.is_cjs || result.side_effects || !result.truly_treeshakeable)
}

/// A budget is judged against a **size**, and only a measured import has one (ADR-0006, invariant
/// 5: no verdict from a floor). This used to ask `result.error.is_none()` — the negative check —
/// which a transiently-degraded result with a fabricated size passed, so the report claimed a
/// violation, or absolved one, on a number that never happened.
fn is_import_budget_violation(
    item: &WorkspaceReportItem,
    budgets: &WorkspaceReportBudgets,
) -> bool {
    match (measured_brotli(item), budgets.per_import_brotli_bytes) {
        (Some(brotli_bytes), Some(limit)) => brotli_bytes > limit,
        _ => false,
    }
}

fn measured_brotli(item: &WorkspaceReportItem) -> Option<u64> {
    item.result.as_ref().and_then(ImportResult::brotli_bytes)
}

fn row_for_item(
    item: &WorkspaceReportItem,
    budgets: &WorkspaceReportBudgets,
) -> WorkspaceReportRow {
    let result = item.result.as_ref();
    // `.and_then(...)`, never `.unwrap_or_default()`: an unmeasured import has NO size, and a
    // zero here prints "0 B" in the exported report — the sentinel this model exists to abolish.
    WorkspaceReportRow {
        package_name: item.detected.package_name.clone(),
        specifier: item.detected.specifier.clone(),
        source_file: relative_source_file(&item.workspace_root, &item.source_file),
        line: item.detected.line + 1,
        runtime: item.detected.runtime.as_str().to_owned(),
        minified_bytes: result.and_then(ImportResult::minified_bytes),
        gzip_bytes: result.and_then(ImportResult::gzip_bytes),
        brotli_bytes: result.and_then(ImportResult::brotli_bytes),
        zstd_bytes: result.and_then(ImportResult::zstd_bytes),
        shared_bytes: result
            .and_then(|item| item.shared_bytes)
            .unwrap_or_default(),
        confidence: confidence_for_result(result),
        confidence_reasons: result
            .map(|item| item.confidence_reasons.join(" \u{b7} "))
            .unwrap_or_default(),
        top_modules: module_breakdown_summary(result),
        warning: warning_for_item(item, budgets),
        module_contributions: result
            .and_then(|item| item.module_breakdown.clone())
            .unwrap_or_default(),
    }
}

fn confidence_for_result(result: Option<&ImportResult>) -> String {
    match result.map(|item| item.confidence) {
        Some(ConfidenceLevel::High) => "high",
        Some(ConfidenceLevel::Medium) => "medium",
        Some(ConfidenceLevel::Low) => "low",
        None => "unknown",
    }
    .to_owned()
}

fn module_breakdown_summary(result: Option<&ImportResult>) -> String {
    result
        .and_then(|item| item.module_breakdown.as_ref())
        .map(|modules| {
            modules
                .iter()
                .take(3)
                .map(|module| format!("{} ({} B)", basename(&module.path), module.bytes))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn warning_for_item(item: &WorkspaceReportItem, budgets: &WorkspaceReportBudgets) -> String {
    let mut warnings = Vec::new();
    if let Some(warning) = item.warning.as_ref() {
        warnings.push(warning.clone());
    }
    if let Some(error) = item
        .result
        .as_ref()
        .and_then(|result| result.error.as_ref())
    {
        warnings.push(error.clone());
    }
    if item
        .result
        .as_ref()
        .and_then(|result| result.shared_bytes)
        .unwrap_or_default()
        > 0
    {
        warnings.push(format!(
            "Shares {} B with other imports in this file",
            item.result
                .as_ref()
                .and_then(|result| result.shared_bytes)
                .unwrap_or_default()
        ));
    }
    if is_import_budget_violation(item, budgets)
        && let (Some(brotli_bytes), Some(limit)) =
            (measured_brotli(item), budgets.per_import_brotli_bytes)
    {
        warnings.push(format!(
            "Budget exceeded: {brotli_bytes} B br > {limit} B br"
        ));
    }
    if is_conservative_item(item) {
        warnings.push("Conservative estimate".to_owned());
    }
    if let Some(result) = item.result.as_ref() {
        match result.confidence {
            ConfidenceLevel::Low => warnings.push(format!(
                "Low confidence{}",
                confidence_reason_suffix(result)
            )),
            ConfidenceLevel::Medium => warnings.push(format!(
                "Medium confidence{}",
                confidence_reason_suffix(result)
            )),
            ConfidenceLevel::High => {}
        }
    }
    warnings.join(" \u{b7} ")
}

/// Appends "File budget exceeded" to the first row of each over-budget file
/// and returns index-aligned flags marking which rows were warned. The flags
/// feed `budget_violation_count`: the TS `/budget exceeded/i` counter matches
/// file-budget warnings as well as per-import ones.
fn apply_file_budget_warnings(
    rows: &mut [WorkspaceReportRow],
    budgets: &WorkspaceReportBudgets,
) -> Vec<bool> {
    let mut flags = vec![false; rows.len()];
    let Some(limit) = budgets.per_file_brotli_bytes else {
        return flags;
    };
    let mut totals = BTreeMap::<String, u64>::new();
    for row in rows.iter() {
        // A file's total is the sum of its MEASURED imports. An unmeasured one adds nothing —
        // which makes the total a floor, and a floor can still exceed a budget (that verdict is
        // sound: the real number is at least this large). It cannot *absolve* one, and it does not
        // try to: an under-budget floor produces no warning either way.
        if let Some(brotli_bytes) = row.brotli_bytes.filter(|bytes| *bytes > 0) {
            *totals.entry(row.source_file.clone()).or_default() += brotli_bytes;
        }
    }
    let mut warned_files = BTreeSet::<String>::new();
    for (index, row) in rows.iter_mut().enumerate() {
        let total = totals.get(&row.source_file).copied().unwrap_or_default();
        if total > limit && warned_files.insert(row.source_file.clone()) {
            row.warning = append_warning(
                &row.warning,
                &format!("File budget exceeded: {total} B br > {limit} B br"),
            );
            flags[index] = true;
        }
    }
    flags
}

fn append_warning(existing: &str, next: &str) -> String {
    if existing.is_empty() {
        next.to_owned()
    } else {
        format!("{existing} \u{b7} {next}")
    }
}

fn confidence_reason_suffix(result: &ImportResult) -> String {
    if result.confidence_reasons.is_empty() {
        String::new()
    } else {
        format!(": {}", result.confidence_reasons.join(" \u{b7} "))
    }
}

fn build_duplicate_import_groups(rows: &[WorkspaceReportRow]) -> Vec<DuplicateImportGroup> {
    let mut groups = BTreeMap::<String, DuplicateImportGroup>::new();
    for row in rows {
        let group = groups
            .entry(row.specifier.clone())
            .or_insert_with(|| DuplicateImportGroup {
                specifier: row.specifier.clone(),
                count: 0,
                total_brotli_bytes: 0,
                source_files: Vec::new(),
            });
        group.count += 1;
        if let Some(brotli_bytes) = row.brotli_bytes {
            group.total_brotli_bytes += brotli_bytes;
        }
        group.source_files.push(row.source_file.clone());
    }
    let mut groups = groups
        .into_values()
        .filter(|group| group.count > 1)
        .map(|mut group| {
            group.source_files = group
                .source_files
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            group
        })
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| right.total_brotli_bytes.cmp(&left.total_brotli_bytes))
            .then_with(|| left.specifier.cmp(&right.specifier))
    });
    groups
}

fn build_duplicate_module_groups(rows: &[WorkspaceReportRow]) -> Vec<DuplicateModuleGroup> {
    let mut groups = BTreeMap::<String, DuplicateModuleGroup>::new();
    for row in rows {
        for module in &row.module_contributions {
            let group = groups
                .entry(module.path.clone())
                .or_insert_with(|| DuplicateModuleGroup {
                    module_path: module.path.clone(),
                    basename: basename(&module.path),
                    count: 0,
                    total_bytes: 0,
                    specifiers: Vec::new(),
                    vendored: is_vendored_module_path(&module.path),
                });
            group.count += 1;
            group.total_bytes += module.bytes;
            group.specifiers.push(row.specifier.clone());
        }
    }
    let mut groups = groups
        .into_values()
        .filter(|group| group.count > 1)
        .map(|mut group| {
            group.specifiers = group
                .specifiers
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            group
        })
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| right.total_bytes.cmp(&left.total_bytes))
            .then_with(|| left.module_path.cmp(&right.module_path))
    });
    groups
}

fn build_treemap(
    rows: &[WorkspaceReportRow],
    total_brotli_bytes: u64,
) -> Vec<WorkspaceReportTreemapItem> {
    rows.iter()
        // A treemap slices a whole into parts. An import with no size has no part to slice, so it
        // is absent rather than drawn as a zero-width sliver.
        .filter_map(|row| {
            row.brotli_bytes
                .filter(|bytes| *bytes > 0)
                .map(|bytes| (row, bytes))
        })
        .take(10)
        .map(|(row, brotli_bytes)| WorkspaceReportTreemapItem {
            package_name: row.package_name.clone(),
            specifier: row.specifier.clone(),
            source_file: row.source_file.clone(),
            brotli_bytes,
            percentage: ((brotli_bytes * 100) + (total_brotli_bytes / 2))
                .checked_div(total_brotli_bytes)
                .unwrap_or(0),
            confidence: row.confidence.clone(),
        })
        .collect()
}

fn relative_source_file(workspace_root: &str, source_file: &str) -> String {
    Path::new(source_file)
        .strip_prefix(workspace_root)
        .ok()
        .and_then(|path| path.to_str())
        .unwrap_or(source_file)
        .replace('\\', "/")
        .trim_start_matches('/')
        .to_owned()
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_owned()
}

fn is_vendored_module_path(module_path: &str) -> bool {
    let normalized = module_path.replace('\\', "/");
    normalized.contains("/vendor/")
        || normalized.contains("/vendors/")
        || normalized.contains("/vendored/")
        || normalized.contains("/node_modules/") && normalized.matches("/node_modules/").count() > 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::{ImportKind, ImportRuntime, ImportSyntax, ModuleContribution};

    fn detected(specifier: &str, line: u32) -> DetectedImport {
        DetectedImport {
            specifier: specifier.to_owned(),
            package_name: specifier.to_owned(),
            named: Vec::new(),
            import_kind: ImportKind::Named,
            syntax: ImportSyntax::Static,
            runtime: ImportRuntime::Component,
            line,
            quote_end: Default::default(),
            specifier_range: Default::default(),
            statement_range: Default::default(),
        }
    }

    fn ok_result(specifier: &str, brotli_bytes: u64) -> ImportResult {
        let mut result = ImportResult::measured(
            specifier,
            crate::ipc::protocol::MeasuredSizes {
                raw_bytes: brotli_bytes,
                minified_bytes: brotli_bytes,
                gzip_bytes: brotli_bytes,
                brotli_bytes,
                zstd_bytes: brotli_bytes,
            },
        );
        result.truly_treeshakeable = true;
        result.confidence = ConfidenceLevel::High;
        result
    }

    fn report_item(
        specifier: &str,
        line: u32,
        source_file: &str,
        result: Option<ImportResult>,
    ) -> WorkspaceReportItem {
        WorkspaceReportItem {
            detected: detected(specifier, line),
            source_file: format!("C:/ws/{source_file}"),
            workspace_root: "C:/ws".to_owned(),
            result,
            warning: None,
        }
    }

    fn no_budgets() -> WorkspaceReportBudgets {
        WorkspaceReportBudgets {
            per_import_brotli_bytes: None,
            per_file_brotli_bytes: None,
        }
    }

    #[test]
    fn summary_counts_conservative_rows_from_structured_result_facts() {
        let mut conservative = ok_result("heavy", 5);
        conservative.side_effects = true;
        let items = vec![
            report_item("heavy", 0, "src/a.ts", Some(conservative)),
            report_item("clean", 1, "src/a.ts", Some(ok_result("clean", 5))),
        ];

        let row_set = build_report_rows(&items, &no_budgets());
        let summary = build_report_summary(&row_set);

        assert_eq!(summary.conservative_count, 1);
        let conservative_row = row_set
            .rows
            .iter()
            .find(|row| row.specifier == "heavy")
            .expect("conservative row");
        assert!(conservative_row.warning.contains("Conservative estimate"));
    }

    #[test]
    fn summary_counts_per_import_budget_violations_structurally() {
        let items = vec![
            report_item("big", 0, "src/a.ts", Some(ok_result("big", 20))),
            report_item(
                "broken",
                1,
                "src/a.ts",
                Some(ImportResult::unmeasured(
                    "broken",
                    "parse",
                    "Package not found",
                    Vec::new(),
                )),
            ),
            report_item("small", 2, "src/a.ts", Some(ok_result("small", 5))),
        ];
        let budgets = WorkspaceReportBudgets {
            per_import_brotli_bytes: Some(10),
            per_file_brotli_bytes: None,
        };

        let row_set = build_report_rows(&items, &budgets);
        let summary = build_report_summary(&row_set);

        // An unmeasured import has no size, so there is no verdict to reach about it.
        assert_eq!(summary.budget_violation_count, 1);
        let violating_row = row_set
            .rows
            .iter()
            .find(|row| row.specifier == "big")
            .expect("violating row");
        assert!(violating_row.warning.contains("Budget exceeded"));
    }

    /// ADR-0006 §6: `.flatten().unwrap_or_default()` on these fields compiles and prints **"0 B"**
    /// in an exported, shared report — the sentinel zero the whole model exists to abolish. The row
    /// carries no number at all, and no aggregate counts it.
    #[test]
    fn an_unmeasured_import_has_no_size_in_the_report_not_a_zero() {
        let items = vec![
            report_item("measured", 0, "src/a.ts", Some(ok_result("measured", 40))),
            report_item(
                "broken",
                1,
                "src/a.ts",
                Some(ImportResult::unmeasured(
                    "broken",
                    "timeout",
                    "engine build did not complete",
                    Vec::new(),
                )),
            ),
        ];

        let row_set = build_report_rows(&items, &no_budgets());
        let summary = build_report_summary(&row_set);
        let broken = row_set
            .rows
            .iter()
            .find(|row| row.specifier == "broken")
            .expect("unmeasured row");

        assert_eq!(broken.brotli_bytes, None);
        assert_eq!(broken.minified_bytes, None);
        assert_eq!(broken.gzip_bytes, None);
        assert_eq!(broken.zstd_bytes, None);
        assert_eq!(
            summary.total_brotli_bytes, 40,
            "the workspace total is the sum of what was measured, and nothing else"
        );
        assert!(
            summary
                .treemap
                .iter()
                .all(|item| item.specifier != "broken"),
            "an import with no size has no slice of the treemap"
        );
    }

    #[test]
    fn summary_counts_file_budget_rows_as_violations() {
        let items = vec![
            report_item("a", 0, "src/a.ts", Some(ok_result("a", 8))),
            report_item("b", 1, "src/a.ts", Some(ok_result("b", 8))),
        ];
        let budgets = WorkspaceReportBudgets {
            per_import_brotli_bytes: None,
            per_file_brotli_bytes: Some(10),
        };

        let row_set = build_report_rows(&items, &budgets);
        let summary = build_report_summary(&row_set);

        // TS counts `/budget exceeded/i`, which matches the per-file
        // "File budget exceeded" warning; only the first row of the file
        // receives that warning, so the count is one, not two.
        assert_eq!(summary.budget_violation_count, 1);
        let warned_rows = row_set
            .rows
            .iter()
            .filter(|row| row.warning.contains("File budget exceeded"))
            .count();
        assert_eq!(warned_rows, 1);
    }

    #[test]
    fn summary_skips_rows_without_violations() {
        let items = vec![report_item(
            "clean",
            0,
            "src/a.ts",
            Some(ok_result("clean", 5)),
        )];
        let budgets = WorkspaceReportBudgets {
            per_import_brotli_bytes: Some(100),
            per_file_brotli_bytes: Some(100),
        };

        let row_set = build_report_rows(&items, &budgets);
        let summary = build_report_summary(&row_set);

        assert_eq!(summary.conservative_count, 0);
        assert_eq!(summary.budget_violation_count, 0);
        assert!(row_set.rows[0].warning.is_empty());
    }

    #[test]
    fn duplicate_import_groups_require_multiple_rows_and_dedupe_sources() {
        let items = vec![
            report_item("dup", 0, "src/a.ts", Some(ok_result("dup", 4))),
            report_item("dup", 5, "src/a.ts", Some(ok_result("dup", 4))),
            report_item("dup", 0, "src/b.ts", Some(ok_result("dup", 4))),
            report_item("single", 1, "src/a.ts", Some(ok_result("single", 4))),
        ];

        let row_set = build_report_rows(&items, &no_budgets());
        let summary = build_report_summary(&row_set);

        assert_eq!(summary.duplicate_imports.len(), 1);
        let group = &summary.duplicate_imports[0];
        assert_eq!(group.specifier, "dup");
        assert_eq!(group.count, 3);
        assert_eq!(group.total_brotli_bytes, 12);
        assert_eq!(group.source_files, vec!["src/a.ts", "src/b.ts"]);
    }

    #[test]
    fn duplicate_module_groups_flag_vendored_paths() {
        let shared_path = "C:/ws/node_modules/a/node_modules/b/index.js";
        let mut first = ok_result("a", 4);
        first.module_breakdown = Some(vec![
            ModuleContribution {
                path: shared_path.to_owned(),
                bytes: 3,
            },
            ModuleContribution {
                path: "C:/ws/node_modules/a/only-once.js".to_owned(),
                bytes: 1,
            },
        ]);
        let mut second = ok_result("b", 4);
        second.module_breakdown = Some(vec![ModuleContribution {
            path: shared_path.to_owned(),
            bytes: 3,
        }]);
        let items = vec![
            report_item("a", 0, "src/a.ts", Some(first)),
            report_item("b", 1, "src/a.ts", Some(second)),
        ];

        let row_set = build_report_rows(&items, &no_budgets());
        let summary = build_report_summary(&row_set);

        assert_eq!(summary.shared_modules.len(), 1);
        let group = &summary.shared_modules[0];
        assert_eq!(group.module_path, shared_path);
        assert_eq!(group.basename, "index.js");
        assert_eq!(group.count, 2);
        assert_eq!(group.total_bytes, 6);
        assert_eq!(group.specifiers, vec!["a", "b"]);
        assert!(group.vendored, "nested node_modules paths are vendored");
    }

    #[test]
    fn treemap_limits_to_top_ten_positive_rows_and_rounds_percentages() {
        let mut items = vec![report_item(
            "big",
            0,
            "src/a.ts",
            Some(ok_result("big", 100)),
        )];
        for index in 0..10 {
            items.push(report_item(
                &format!("mid{index}"),
                index + 1,
                "src/a.ts",
                Some(ok_result("mid", 10)),
            ));
        }
        items.push(report_item(
            "zero",
            20,
            "src/a.ts",
            Some(ok_result("zero", 0)),
        ));

        let row_set = build_report_rows(&items, &no_budgets());
        let summary = build_report_summary(&row_set);

        assert_eq!(summary.treemap.len(), 10);
        assert_eq!(summary.treemap[0].specifier, "big");
        assert_eq!(summary.treemap[0].percentage, 50);
        assert_eq!(summary.treemap[1].percentage, 5);
        assert!(
            summary
                .treemap
                .iter()
                .all(|treemap_item| treemap_item.brotli_bytes > 0)
        );
    }

    #[test]
    fn treemap_rounds_percentages_half_up_like_math_round() {
        let items = vec![
            report_item("two", 0, "src/a.ts", Some(ok_result("two", 2))),
            report_item("one", 1, "src/a.ts", Some(ok_result("one", 1))),
        ];

        let row_set = build_report_rows(&items, &no_budgets());
        let summary = build_report_summary(&row_set);

        // total = 3: Math.round(66.67) = 67 and Math.round(33.33) = 33.
        assert_eq!(summary.treemap[0].percentage, 67);
        assert_eq!(summary.treemap[1].percentage, 33);
    }

    #[test]
    fn treemap_with_zero_total_reports_zero_percentages() {
        let items = vec![report_item("a", 0, "src/a.ts", Some(ok_result("a", 5)))];
        let row_set = build_report_rows(&items, &no_budgets());

        let treemap = build_treemap(&row_set.rows, 0);

        assert_eq!(treemap.len(), 1);
        assert_eq!(treemap[0].percentage, 0);
    }
}
