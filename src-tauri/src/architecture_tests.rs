#![cfg(test)]

use regex::Regex;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use syn::visit::{self, Visit};
use syn::{
    Attribute, ExprLit, ImplItem, Item, ItemImpl, ItemStruct, ItemUse, Lit, Meta, Path as SynPath,
    Token, Type, UseTree,
};

static PROVIDER_DML: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)\b(?:INSERT(?:\s+OR\s+(?:ABORT|FAIL|IGNORE|REPLACE|ROLLBACK))?\s+INTO|UPDATE(?:\s+OR\s+(?:ABORT|FAIL|IGNORE|REPLACE|ROLLBACK))?|DELETE\s+FROM)\s+["`\[]?(providers|provider_endpoints)\b"#,
    )
    .expect("compile provider DML classifier")
});
static RESTORE_CREATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\bCREATE\s+(?:TEMP(?:ORARY)?\s+)?(?:TABLE|INDEX|TRIGGER|VIEW)\b")
        .expect("compile restore DDL classifier")
});

#[derive(Debug, Clone, PartialEq, Eq)]
struct Violation {
    kind: &'static str,
    path: String,
    detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CfgTruth {
    True,
    False,
    Unknown,
}

impl CfgTruth {
    fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }

    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, Self::True) => Self::True,
            _ => Self::Unknown,
        }
    }

    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, Self::False) => Self::False,
            _ => Self::Unknown,
        }
    }
}

/// Evaluate a cfg predicate for a production build where `test = false`.
/// Other target/features are deliberately unknown: the scanner may exclude an
/// item only when the predicate is definitely false in every production
/// configuration.
fn production_cfg_truth(meta: &Meta) -> CfgTruth {
    match meta {
        Meta::Path(path) if path.is_ident("test") => CfgTruth::False,
        Meta::Path(_) | Meta::NameValue(_) => CfgTruth::Unknown,
        Meta::List(list) if list.path.is_ident("not") => list
            .parse_args::<Meta>()
            .map(|inner| production_cfg_truth(&inner).not())
            .unwrap_or(CfgTruth::Unknown),
        Meta::List(list) if list.path.is_ident("all") || list.path.is_ident("any") => {
            let Ok(items) = list
                .parse_args_with(syn::punctuated::Punctuated::<Meta, Token![,]>::parse_terminated)
            else {
                return CfgTruth::Unknown;
            };
            if list.path.is_ident("all") {
                items.iter().fold(CfgTruth::True, |truth, item| {
                    truth.and(production_cfg_truth(item))
                })
            } else {
                items.iter().fold(CfgTruth::False, |truth, item| {
                    truth.or(production_cfg_truth(item))
                })
            }
        }
        Meta::List(_) => CfgTruth::Unknown,
    }
}

fn is_cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attribute| {
        let Meta::List(list) = &attribute.meta else {
            return false;
        };
        if !list.path.is_ident("cfg") {
            return false;
        }
        list.parse_args::<Meta>()
            .map(|predicate| production_cfg_truth(&predicate) == CfgTruth::False)
            .unwrap_or(false)
    })
}

fn flatten_use(tree: &UseTree, prefix: &mut Vec<String>, output: &mut Vec<Vec<String>>) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            flatten_use(&path.tree, prefix, output);
            prefix.pop();
        }
        UseTree::Name(name) => {
            let mut path = prefix.clone();
            path.push(name.ident.to_string());
            output.push(path);
        }
        UseTree::Rename(rename) => {
            let mut path = prefix.clone();
            path.push(rename.ident.to_string());
            output.push(path);
        }
        UseTree::Group(group) => {
            for item in &group.items {
                flatten_use(item, prefix, output);
            }
        }
        UseTree::Glob(_) => output.push(prefix.clone()),
    }
}

fn path_segments(path: &SynPath) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect()
}

struct ArchitectureVisitor<'a> {
    path: &'a str,
    source_module: Option<&'static str>,
    violations: Vec<Violation>,
    internal_edges: BTreeSet<String>,
}

fn pi_config_source_module(path: &str) -> Option<&'static str> {
    ["raw_schema", "composer", "gateway", "native", "model"]
        .into_iter()
        .find(|module| {
            let root = format!("pi_config/{module}");
            path.ends_with(&format!("{root}.rs")) || path.contains(&format!("{root}/"))
        })
}

impl ArchitectureVisitor<'_> {
    fn record_dependency(&mut self, segments: &[String]) {
        let Some(source_module) = self.source_module else {
            return;
        };
        let qualified_internal_path = segments.len() > 1
            && segments.iter().any(|segment| {
                matches!(segment.as_str(), "super" | "self" | "crate" | "pi_config")
            });
        if qualified_internal_path {
            for module in [
                "raw_schema",
                "composer",
                "gateway",
                "native",
                "model",
                "document",
            ] {
                if module != source_module && segments.iter().any(|segment| segment == module) {
                    self.internal_edges
                        .insert(format!("{source_module}->{module}"));
                }
            }
        }

        let imports_gateway = segments
            .iter()
            .any(|segment| segment == "gateway" || segment.starts_with("PiGateway"));
        let imports_model_module = segments.len() > 1
            && segments.iter().any(|segment| segment == "model")
            && segments.iter().any(|segment| {
                matches!(segment.as_str(), "super" | "self" | "crate" | "pi_config")
            });
        let imports_managed = imports_model_module
            || segments.iter().any(|segment| {
                segment == "PiApiFamily"
                    || segment.starts_with("PiManaged")
                    || segment == "PiEffectiveModel"
            });
        if matches!(source_module, "raw_schema" | "composer")
            && (imports_gateway || imports_managed)
        {
            self.violations.push(Violation {
                kind: "cross_layer_import",
                path: self.path.to_string(),
                detail: format!(
                    "{source_module} imports managed/gateway path {}",
                    segments.join("::")
                ),
            });
        }

        let imports_raw_valid = segments
            .iter()
            .any(|segment| segment == "raw_schema" || segment == "PiRawValidProvider");
        if source_module == "gateway" && imports_raw_valid {
            self.violations.push(Violation {
                kind: "cross_layer_import",
                path: self.path.to_string(),
                detail: format!(
                    "gateway constructs or imports raw-valid path {}",
                    segments.join("::")
                ),
            });
        }
    }

    fn inspect_string(&mut self, literal: &str) {
        if PROVIDER_DML.is_match(literal) && !provider_dml_path_allowed(self.path) {
            self.violations.push(Violation {
                kind: "provider_dml",
                path: self.path.to_string(),
                detail: "provider table DML exists outside typed row/state, endpoint, migration, or canonical-copy authority".to_string(),
            });
        }
        if self.path.ends_with("database/backup.rs") && RESTORE_CREATE.is_match(literal) {
            self.violations.push(Violation {
                kind: "restore_create",
                path: self.path.to_string(),
                detail: "restore production code contains CREATE TABLE/INDEX/TRIGGER/VIEW"
                    .to_string(),
            });
        }
    }
}

impl<'ast> Visit<'ast> for ArchitectureVisitor<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        let attrs = match item {
            Item::Const(item) => &item.attrs,
            Item::Enum(item) => &item.attrs,
            Item::ExternCrate(item) => &item.attrs,
            Item::Fn(item) => &item.attrs,
            Item::ForeignMod(item) => &item.attrs,
            Item::Impl(item) => &item.attrs,
            Item::Macro(item) => &item.attrs,
            Item::Mod(item) => &item.attrs,
            Item::Static(item) => &item.attrs,
            Item::Struct(item) => &item.attrs,
            Item::Trait(item) => &item.attrs,
            Item::TraitAlias(item) => &item.attrs,
            Item::Type(item) => &item.attrs,
            Item::Union(item) => &item.attrs,
            Item::Use(item) => &item.attrs,
            Item::Verbatim(_) => {
                visit::visit_item(self, item);
                return;
            }
            _ => {
                visit::visit_item(self, item);
                return;
            }
        };
        if !is_cfg_test(attrs) {
            visit::visit_item(self, item);
        }
    }

    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        let attrs = match item {
            ImplItem::Const(item) => &item.attrs,
            ImplItem::Fn(item) => &item.attrs,
            ImplItem::Type(item) => &item.attrs,
            ImplItem::Macro(item) => &item.attrs,
            ImplItem::Verbatim(_) => {
                visit::visit_impl_item(self, item);
                return;
            }
            _ => {
                visit::visit_impl_item(self, item);
                return;
            }
        };
        if !is_cfg_test(attrs) {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_ident(&mut self, identifier: &'ast syn::Ident) {
        let identifier = identifier.to_string();
        if matches!(
            identifier.as_str(),
            "save_provider" | "save_provider_row_on_tx" | "save_provider_aggregate"
        ) || identifier.starts_with("upsert_provider")
        {
            self.violations.push(Violation {
                kind: "forbidden_provider_symbol",
                path: self.path.to_string(),
                detail: format!("forbidden provider write symbol '{identifier}'"),
            });
        }
    }

    fn visit_expr_lit(&mut self, expression: &'ast ExprLit) {
        if let Lit::Str(literal) = &expression.lit {
            self.inspect_string(&literal.value());
        }
        visit::visit_expr_lit(self, expression);
    }

    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        let mut paths = Vec::new();
        flatten_use(&item.tree, &mut Vec::new(), &mut paths);
        for path in paths {
            self.record_dependency(&path);
        }
        visit::visit_item_use(self, item);
    }

    fn visit_path(&mut self, path: &'ast SynPath) {
        self.record_dependency(&path_segments(path));
        visit::visit_path(self, path);
    }
}

fn provider_dml_path_allowed(path: &str) -> bool {
    [
        "database/dao/provider_write.rs",
        "database/dao/providers.rs",
        "database/dao/failover.rs",
        "database/schema.rs",
        "database/migration.rs",
        "database/backup.rs",
    ]
    .iter()
    .any(|allowed| path.ends_with(allowed))
}

fn scan_source(path: &str, source: &str) -> (Vec<Violation>, BTreeSet<String>) {
    scan_source_as_module(path, source, pi_config_source_module(path))
}

fn scan_source_as_module(
    path: &str,
    source: &str,
    source_module: Option<&'static str>,
) -> (Vec<Violation>, BTreeSet<String>) {
    let syntax = match syn::parse_file(source) {
        Ok(syntax) => syntax,
        Err(error) => {
            return (
                vec![Violation {
                    kind: "parse_error",
                    path: path.to_string(),
                    detail: error.to_string(),
                }],
                BTreeSet::new(),
            )
        }
    };
    // 文件级 #![cfg(test)] 的文件(认证套件等)不进入任何构建的生产目标,
    // 不参与生产扫描;该属性的存在性由认证套件的注册元测试强制。
    if is_cfg_test(&syntax.attrs) {
        return (Vec::new(), BTreeSet::new());
    }
    let mut visitor = ArchitectureVisitor {
        path,
        source_module,
        violations: Vec::new(),
        internal_edges: BTreeSet::new(),
    };
    visitor.visit_file(&syntax);
    (visitor.violations, visitor.internal_edges)
}

fn rust_sources(root: &Path) -> Vec<PathBuf> {
    fn walk(directory: &Path, output: &mut Vec<PathBuf>) {
        let mut entries = fs::read_dir(directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|error| panic!("read entry in {}: {error}", directory.display()));
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, output);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                output.push(path);
            }
        }
    }
    let mut output = Vec::new();
    walk(root, &mut output);
    output
}

#[derive(Debug, Default)]
struct ModulePathOptions {
    explicit: Vec<PathBuf>,
    definitely_explicit: bool,
}

impl ModulePathOptions {
    fn default_path_is_possible(&self) -> bool {
        !self.definitely_explicit
    }
}

fn collect_conditional_module_paths(
    meta: &Meta,
    activation: CfgTruth,
    output: &mut ModulePathOptions,
) {
    match meta {
        Meta::NameValue(value) if value.path.is_ident("path") => {
            let syn::Expr::Lit(expression) = &value.value else {
                return;
            };
            let Lit::Str(path) = &expression.lit else {
                return;
            };
            if activation != CfgTruth::False {
                output.explicit.push(PathBuf::from(path.value()));
                output.definitely_explicit |= activation == CfgTruth::True;
            }
        }
        Meta::List(list) if list.path.is_ident("cfg_attr") => {
            let Ok(items) = list
                .parse_args_with(syn::punctuated::Punctuated::<Meta, Token![,]>::parse_terminated)
            else {
                return;
            };
            let mut items = items.iter();
            let Some(predicate) = items.next() else {
                return;
            };
            let activation = activation.and(production_cfg_truth(predicate));
            for attribute in items {
                collect_conditional_module_paths(attribute, activation, output);
            }
        }
        _ => {}
    }
}

fn module_path_options(attrs: &[Attribute]) -> ModulePathOptions {
    let mut output = ModulePathOptions::default();
    for attribute in attrs {
        collect_conditional_module_paths(&attribute.meta, CfgTruth::True, &mut output);
    }
    output.explicit.sort();
    output.explicit.dedup();
    output
}

fn default_submodule_directory(source_path: &Path) -> PathBuf {
    let parent = source_path
        .parent()
        .expect("Rust source has a parent directory");
    match source_path.file_stem().and_then(|stem| stem.to_str()) {
        Some("lib" | "main" | "mod") | None => parent.to_path_buf(),
        Some(stem) => parent.join(stem),
    }
}

fn collect_module_targets(
    items: &[Item],
    source_path: &Path,
    inline_modules: &[String],
    output: &mut Vec<PathBuf>,
) {
    for item in items {
        let Item::Mod(module) = item else {
            continue;
        };
        if is_cfg_test(&module.attrs) {
            continue;
        }
        if let Some((_, nested)) = &module.content {
            let mut nested_modules = inline_modules.to_vec();
            nested_modules.push(module.ident.to_string());
            collect_module_targets(nested, source_path, &nested_modules, output);
            continue;
        }

        let options = module_path_options(&module.attrs);
        let module_directory = default_submodule_directory(source_path);
        let inline_directory = inline_modules
            .iter()
            .fold(module_directory, |directory, module| directory.join(module));
        let explicit_base = if inline_modules.is_empty() {
            source_path
                .parent()
                .expect("Rust source has a parent directory")
                .to_path_buf()
        } else {
            inline_directory.clone()
        };
        let default_path_is_possible = options.default_path_is_possible();
        output.extend(
            options
                .explicit
                .into_iter()
                .map(|relative| explicit_base.join(relative)),
        );
        if default_path_is_possible {
            let module_name = module.ident.to_string();
            output.push(inline_directory.join(format!("{module_name}.rs")));
            output.push(inline_directory.join(module_name).join("mod.rs"));
        }
    }
}

fn inherited_module_owners(
    sources: &[(PathBuf, String, String)],
) -> BTreeMap<PathBuf, BTreeSet<&'static str>> {
    let mut owners = BTreeMap::<PathBuf, BTreeSet<&'static str>>::new();
    for (path, relative, _) in sources {
        if let Some(owner) = pi_config_source_module(relative) {
            owners
                .entry(fs::canonicalize(path).expect("canonicalize Rust source"))
                .or_default()
                .insert(owner);
        }
    }

    loop {
        let mut changed = false;
        for (path, _, source) in sources {
            let canonical = fs::canonicalize(path).expect("canonicalize Rust source");
            let source_owners = owners.get(&canonical).cloned().unwrap_or_default();
            if source_owners.is_empty() {
                continue;
            }
            let Ok(syntax) = syn::parse_file(source) else {
                continue;
            };
            if is_cfg_test(&syntax.attrs) {
                continue;
            }
            let mut targets = Vec::new();
            collect_module_targets(&syntax.items, path, &[], &mut targets);
            for target in targets {
                let Ok(target) = fs::canonicalize(target) else {
                    continue;
                };
                let target_owners = owners.entry(target).or_default();
                let before = target_owners.len();
                target_owners.extend(source_owners.iter().copied());
                changed |= target_owners.len() != before;
            }
        }
        if !changed {
            return owners;
        }
    }
}

fn scan_production_tree(
    manifest_dir: &Path,
    source_root: &Path,
) -> (Vec<Violation>, BTreeSet<String>) {
    let mut violations = Vec::new();
    let mut edges = BTreeSet::new();
    let sources = rust_sources(source_root)
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(manifest_dir)
                .expect("source is under manifest")
                .to_string_lossy()
                .replace('\\', "/");
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            (path, relative, source)
        })
        .collect::<Vec<_>>();
    let owners = inherited_module_owners(&sources);
    let scanned_paths = sources
        .iter()
        .map(|(path, _, _)| fs::canonicalize(path).expect("canonicalize Rust source"))
        .collect::<BTreeSet<_>>();
    for (target, target_owners) in &owners {
        if !scanned_paths.contains(target) {
            violations.push(Violation {
                kind: "module_path_outside_scan",
                path: target.display().to_string(),
                detail: format!(
                    "a #[path] module owned by {} escapes the scanned source tree",
                    target_owners.iter().copied().collect::<Vec<_>>().join(",")
                ),
            });
        }
    }
    for (path, relative, source) in sources {
        let canonical = fs::canonicalize(&path).expect("canonicalize Rust source");
        let inherited = owners.get(&canonical).cloned().unwrap_or_default();
        if inherited.is_empty() {
            let (mut source_violations, source_edges) = scan_source(&relative, &source);
            violations.append(&mut source_violations);
            edges.extend(source_edges);
        } else {
            for owner in inherited {
                let (mut source_violations, source_edges) =
                    scan_source_as_module(&relative, &source, Some(owner));
                violations.append(&mut source_violations);
                edges.extend(source_edges);
            }
        }
    }
    (violations, edges)
}

fn type_leaf(ty: &Type) -> String {
    match ty {
        Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        Type::Reference(reference) => type_leaf(&reference.elem),
        _ => "other".to_string(),
    }
}

fn provider_write_api_snapshot(source: &str) -> serde_json::Value {
    let syntax = syn::parse_file(source).expect("parse provider write authority");
    let type_names = [
        "ProviderKey",
        "ProviderRowCreate",
        "ProviderRowUpdate",
        "NewEndpoint",
        "NewProviderAggregate",
        "RenameProvider",
    ];
    let method_names = [
        "create_provider",
        "update_provider",
        "rename_db_only_additive_provider",
        "add_provider_endpoint",
        "remove_provider_endpoint",
        "touch_provider_endpoint",
    ];
    let mut types = BTreeMap::<String, Vec<String>>::new();
    let mut methods = BTreeMap::<String, Vec<String>>::new();
    for item in syntax.items {
        if let Item::Struct(ItemStruct { ident, fields, .. }) = &item {
            if type_names.contains(&ident.to_string().as_str()) {
                types.insert(
                    ident.to_string(),
                    fields
                        .iter()
                        .filter_map(|field| field.ident.as_ref().map(ToString::to_string))
                        .collect(),
                );
            }
        }
        if let Item::Impl(ItemImpl { self_ty, items, .. }) = item {
            if type_leaf(&self_ty) != "Database" {
                continue;
            }
            for item in items {
                let ImplItem::Fn(function) = item else {
                    continue;
                };
                let name = function.sig.ident.to_string();
                if !method_names.contains(&name.as_str()) {
                    continue;
                }
                let inputs = function
                    .sig
                    .inputs
                    .iter()
                    .filter_map(|argument| match argument {
                        syn::FnArg::Receiver(_) => None,
                        syn::FnArg::Typed(argument) => Some(type_leaf(&argument.ty)),
                    })
                    .collect();
                methods.insert(name, inputs);
            }
        }
    }
    json!({
        "manifestVersion": 1,
        "codeAuthority": "src-tauri/src/database/dao/provider_write.rs",
        "types": types,
        "databaseMethods": methods,
    })
}

#[test]
fn architecture_scanner_accepts_production_tree_and_matches_machine_snapshots() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_root = manifest_dir.join("src");
    let (violations, edges) = scan_production_tree(manifest_dir, &source_root);
    assert!(
        violations.is_empty(),
        "architecture violations:\n{}",
        violations
            .iter()
            .map(|violation| format!(
                "{} {}: {}",
                violation.kind, violation.path, violation.detail
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Integration tests compile as external consumers and used to retain
    // calls to the deleted generic writer even after `cargo test --lib`
    // passed. They may contain deliberate SQL fixtures, so this pass checks
    // only that the forbidden API surface is absent from actual Rust syntax.
    let mut forbidden_test_symbols = Vec::new();
    for path in rust_sources(&manifest_dir.join("tests")) {
        let relative = path
            .strip_prefix(manifest_dir)
            .expect("integration test is under manifest")
            .to_string_lossy()
            .replace('\\', "/");
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let (source_violations, _) = scan_source(&relative, &source);
        forbidden_test_symbols.extend(
            source_violations
                .into_iter()
                .filter(|violation| violation.kind == "forbidden_provider_symbol"),
        );
    }
    assert!(
        forbidden_test_symbols.is_empty(),
        "forbidden provider write symbols remain in integration tests:\n{}",
        forbidden_test_symbols
            .iter()
            .map(|violation| format!("{}: {}", violation.path, violation.detail))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let module_snapshot: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/pi/module-boundaries-v1.json"
    ))
    .expect("parse module boundary snapshot");
    assert_eq!(
        json!({
            "manifestVersion": 1,
            "codeAuthority": "src-tauri/src/architecture_tests.rs",
            "edges": edges,
        }),
        module_snapshot
    );

    let provider_source = fs::read_to_string(source_root.join("database/dao/provider_write.rs"))
        .expect("read provider write authority");
    let provider_snapshot: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/pi/provider-write-api-v1.json"
    ))
    .expect("parse provider write API snapshot");
    assert_eq!(
        provider_write_api_snapshot(&provider_source),
        provider_snapshot
    );
}

#[test]
fn architecture_scanner_negative_fixtures_prove_each_guard_fires() {
    let cases = [
        (
            "src/services/forbidden.rs",
            "fn call() { save_provider(); }",
            "forbidden_provider_symbol",
        ),
        (
            "src/services/rogue.rs",
            r#"fn write(conn: &Db) { conn.execute("UPDATE OR REPLACE providers SET name = 'x'", []); }"#,
            "provider_dml",
        ),
        (
            "src/database/backup.rs",
            r#"fn stage(conn: &Db) { conn.execute("CREATE TABLE leaked (id INTEGER)", []); }"#,
            "restore_create",
        ),
        (
            "src/pi_config/composer.rs",
            "use super::gateway::PiGatewayApiFamily;",
            "cross_layer_import",
        ),
        (
            "src/pi_config/gateway.rs",
            "use super::raw_schema::PiRawValidProvider;",
            "cross_layer_import",
        ),
    ];
    for (path, source, expected_kind) in cases {
        let (violations, _) = scan_source(path, source);
        assert!(
            violations
                .iter()
                .any(|violation| violation.kind == expected_kind),
            "negative fixture {path} did not trigger {expected_kind}: {violations:?}"
        );
    }

    let ignored_test_fixture = r#"
        #[cfg(test)]
        mod tests {
            fn legacy() {
                save_provider();
                let _ = "CREATE TABLE ignored (id INTEGER)";
                let _ = "DELETE FROM providers";
            }
        }
    "#;
    let (violations, _) = scan_source("src/services/ignored.rs", ignored_test_fixture);
    assert!(
        violations.is_empty(),
        "#[cfg(test)] content must be excluded: {violations:?}"
    );

    let production_not_test_fixture = r#"
        #[cfg(not(test))]
        fn production_escape_attempt() {
            save_provider();
        }
    "#;
    let (violations, _) = scan_source("src/services/production.rs", production_not_test_fixture);
    assert!(
        violations
            .iter()
            .any(|violation| violation.kind == "forbidden_provider_symbol"),
        "#[cfg(not(test))] production content must remain visible: {violations:?}"
    );

    let test_only_conjunction = r#"
        #[cfg(all(test, unix))]
        fn test_only() {
            save_provider();
        }
    "#;
    let (violations, _) = scan_source("src/services/test_only.rs", test_only_conjunction);
    assert!(
        violations.is_empty(),
        "a predicate requiring test must stay excluded: {violations:?}"
    );

    let temp = tempfile::tempdir().expect("temp architecture tree");
    let nested = temp.path().join("src/pi_config/composer/tests/mod.rs");
    fs::create_dir_all(nested.parent().expect("nested parent")).expect("create nested tree");
    fs::write(
        &nested,
        r#"
            use super::super::gateway::PiGatewayApiFamily;
            fn production_escape_attempt() {
                save_provider();
            }
        "#,
    )
    .expect("write production nested-module fixture");
    let (violations, _) = scan_production_tree(temp.path(), &temp.path().join("src"));
    for expected_kind in ["cross_layer_import", "forbidden_provider_symbol"] {
        assert!(
            violations
                .iter()
                .any(|violation| violation.kind == expected_kind),
            "production code under tests/ must inherit composer ownership and trigger \
             {expected_kind}: {violations:?}"
        );
    }

    let custom_path_root = tempfile::tempdir().expect("temp custom-path architecture tree");
    let composer = custom_path_root.path().join("src/pi_config/composer.rs");
    let shared = custom_path_root.path().join("src/shared/helper.rs");
    fs::create_dir_all(composer.parent().expect("composer parent")).expect("create pi_config");
    fs::create_dir_all(shared.parent().expect("shared parent")).expect("create shared");
    fs::write(
        &composer,
        r#"
            #[path = "../shared/helper.rs"]
            mod helper;
        "#,
    )
    .expect("write custom-path parent");
    fs::write(
        &shared,
        r#"
            use crate::pi_config::gateway::PiGatewayApiFamily;
            fn production_escape_attempt() {
                save_provider();
            }
        "#,
    )
    .expect("write custom-path child");
    let (violations, _) = scan_production_tree(
        custom_path_root.path(),
        &custom_path_root.path().join("src"),
    );
    for expected_kind in ["cross_layer_import", "forbidden_provider_symbol"] {
        assert!(
            violations.iter().any(|violation| {
                violation.kind == expected_kind && violation.path.ends_with("src/shared/helper.rs")
            }),
            "#[path] modules must inherit composer ownership and trigger \
             {expected_kind}: {violations:?}"
        );
    }

    let conditional_path_root =
        tempfile::tempdir().expect("temp conditional-path architecture tree");
    let composer = conditional_path_root
        .path()
        .join("src/pi_config/composer.rs");
    let shared = conditional_path_root.path().join("src/shared/helper.rs");
    fs::create_dir_all(composer.parent().expect("composer parent")).expect("create pi_config");
    fs::create_dir_all(shared.parent().expect("shared parent")).expect("create shared");
    fs::write(
        &composer,
        r#"
            #[cfg_attr(not(test), path = "../shared/helper.rs")]
            mod helper;
        "#,
    )
    .expect("write conditional-path parent");
    fs::write(
        &shared,
        r#"
            use crate::pi_config::gateway::PiGatewayApiFamily;
            fn production_escape_attempt() {
                save_provider();
            }
        "#,
    )
    .expect("write conditional-path child");
    let (violations, _) = scan_production_tree(
        conditional_path_root.path(),
        &conditional_path_root.path().join("src"),
    );
    for expected_kind in ["cross_layer_import", "forbidden_provider_symbol"] {
        assert!(
            violations.iter().any(|violation| {
                violation.kind == expected_kind && violation.path.ends_with("src/shared/helper.rs")
            }),
            "production cfg_attr(path) modules must inherit composer ownership and trigger \
             {expected_kind}: {violations:?}"
        );
    }

    let transitive_path_root = tempfile::tempdir().expect("temp transitive-path architecture tree");
    let composer = transitive_path_root
        .path()
        .join("src/pi_config/composer.rs");
    let helper = transitive_path_root.path().join("src/shared/helper.rs");
    let leaf = transitive_path_root
        .path()
        .join("src/shared/helper/leaf.rs");
    fs::create_dir_all(composer.parent().expect("composer parent")).expect("create pi_config");
    fs::create_dir_all(helper.parent().expect("helper parent")).expect("create shared");
    fs::create_dir_all(leaf.parent().expect("leaf parent")).expect("create helper module");
    fs::write(
        &composer,
        r#"
            #[path = "../shared/helper.rs"]
            mod helper;
        "#,
    )
    .expect("write transitive parent");
    fs::write(&helper, "mod leaf;").expect("write transitive child");
    fs::write(
        &leaf,
        r#"
            use crate::pi_config::gateway::PiGatewayApiFamily;
            fn production_escape_attempt() {
                save_provider();
            }
        "#,
    )
    .expect("write transitive leaf");
    let (violations, _) = scan_production_tree(
        transitive_path_root.path(),
        &transitive_path_root.path().join("src"),
    );
    for expected_kind in ["cross_layer_import", "forbidden_provider_symbol"] {
        assert!(
            violations.iter().any(|violation| {
                violation.kind == expected_kind
                    && violation.path.ends_with("src/shared/helper/leaf.rs")
            }),
            "ordinary descendants of #[path] modules must inherit composer ownership and \
             trigger {expected_kind}: {violations:?}"
        );
    }
}
