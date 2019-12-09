//! The core of the module-level name resolution algorithm.
//!
//! `DefCollector::collect` contains the fixed-point iteration loop which
//! resolves imports and expands macros.

use hir_expand::{
    builtin_derive::find_builtin_derive,
    builtin_macro::find_builtin_macro,
    name::{self, AsName, Name},
    HirFileId, MacroCallId, MacroCallKind, MacroDefId, MacroDefKind,
};
use ra_cfg::CfgOptions;
use ra_db::{CrateId, FileId};
use ra_syntax::ast;
use rustc_hash::FxHashMap;
use test_utils::tested_by;

use crate::{
    attr::Attrs,
    db::DefDatabase,
    nameres::{
        diagnostics::DefDiagnostic, mod_resolution::ModDir, path_resolution::ReachedFixedPoint,
        raw, BuiltinShadowMode, CrateDefMap, ModuleData, ModuleOrigin, Resolution, ResolveMode,
    },
    path::{Path, PathKind},
    per_ns::PerNs,
    AdtId, AstId, AstItemDef, ConstLoc, ContainerId, EnumId, EnumVariantId, FunctionLoc, ImplId,
    Intern, LocalImportId, LocalModuleId, LocationCtx, ModuleDefId, ModuleId, StaticLoc, StructId,
    TraitId, TypeAliasLoc, UnionId,
};

pub(super) fn collect_defs(db: &impl DefDatabase, mut def_map: CrateDefMap) -> CrateDefMap {
    let crate_graph = db.crate_graph();

    // populate external prelude
    for dep in crate_graph.dependencies(def_map.krate) {
        let dep_def_map = db.crate_def_map(dep.crate_id);
        log::debug!("crate dep {:?} -> {:?}", dep.name, dep.crate_id);
        def_map.extern_prelude.insert(
            dep.as_name(),
            ModuleId { krate: dep.crate_id, local_id: dep_def_map.root }.into(),
        );

        // look for the prelude
        // If the dependency defines a prelude, we overwrite an already defined
        // prelude. This is necessary to import the "std" prelude if a crate
        // depends on both "core" and "std".
        let dep_def_map = db.crate_def_map(dep.crate_id);
        if dep_def_map.prelude.is_some() {
            def_map.prelude = dep_def_map.prelude;
        }
    }

    let cfg_options = crate_graph.cfg_options(def_map.krate);

    let mut collector = DefCollector {
        db,
        def_map,
        glob_imports: FxHashMap::default(),
        unresolved_imports: Vec::new(),
        resolved_imports: Vec::new(),

        unexpanded_macros: Vec::new(),
        unexpanded_attribute_macros: Vec::new(),
        mod_dirs: FxHashMap::default(),
        cfg_options,
    };
    collector.collect();
    collector.finish()
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum PartialResolvedImport {
    /// None of any namespaces is resolved
    Unresolved,
    /// One of namespaces is resolved
    Indeterminate(PerNs),
    /// All namespaces are resolved, OR it is came from other crate
    Resolved(PerNs),
}

impl PartialResolvedImport {
    fn namespaces(&self) -> PerNs {
        match self {
            PartialResolvedImport::Unresolved => PerNs::none(),
            PartialResolvedImport::Indeterminate(ns) => *ns,
            PartialResolvedImport::Resolved(ns) => *ns,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ImportDirective {
    module_id: LocalModuleId,
    import_id: LocalImportId,
    import: raw::ImportData,
    status: PartialResolvedImport,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MacroDirective {
    module_id: LocalModuleId,
    ast_id: AstId<ast::MacroCall>,
    path: Path,
    legacy: Option<MacroCallId>,
}

/// Walks the tree of module recursively
struct DefCollector<'a, DB> {
    db: &'a DB,
    def_map: CrateDefMap,
    glob_imports: FxHashMap<LocalModuleId, Vec<(LocalModuleId, LocalImportId)>>,
    unresolved_imports: Vec<ImportDirective>,
    resolved_imports: Vec<ImportDirective>,
    unexpanded_macros: Vec<MacroDirective>,
    unexpanded_attribute_macros: Vec<(LocalModuleId, AstId<ast::ModuleItem>, Path)>,
    mod_dirs: FxHashMap<LocalModuleId, ModDir>,
    cfg_options: &'a CfgOptions,
}

impl<DB> DefCollector<'_, DB>
where
    DB: DefDatabase,
{
    fn collect(&mut self) {
        let crate_graph = self.db.crate_graph();
        let file_id = crate_graph.crate_root(self.def_map.krate);
        let raw_items = self.db.raw_items(file_id.into());
        let module_id = self.def_map.root;
        self.def_map.modules[module_id].origin = ModuleOrigin::CrateRoot { definition: file_id };
        ModCollector {
            def_collector: &mut *self,
            module_id,
            file_id: file_id.into(),
            raw_items: &raw_items,
            mod_dir: ModDir::root(),
        }
        .collect(raw_items.items());

        // main name resolution fixed-point loop.
        let mut i = 0;
        loop {
            self.db.check_canceled();
            self.resolve_imports();

            match self.resolve_macros() {
                ReachedFixedPoint::Yes => break,
                ReachedFixedPoint::No => i += 1,
            }
            if i == 1000 {
                log::error!("name resolution is stuck");
                break;
            }
        }

        // Resolve all indeterminate resolved imports again
        // As some of the macros will expand newly import shadowing partial resolved imports
        // FIXME: We maybe could skip this, if we handle the Indetermine imports in `resolve_imports`
        // correctly
        let partial_resolved = self.resolved_imports.iter().filter_map(|directive| {
            if let PartialResolvedImport::Indeterminate(_) = directive.status {
                let mut directive = directive.clone();
                directive.status = PartialResolvedImport::Unresolved;
                Some(directive)
            } else {
                None
            }
        });
        self.unresolved_imports.extend(partial_resolved);
        self.resolve_imports();

        let unresolved_imports = std::mem::replace(&mut self.unresolved_imports, Vec::new());
        // show unresolved imports in completion, etc
        for directive in unresolved_imports {
            self.record_resolved_import(&directive)
        }
    }

    /// Define a macro with `macro_rules`.
    ///
    /// It will define the macro in legacy textual scope, and if it has `#[macro_export]`,
    /// then it is also defined in the root module scope.
    /// You can `use` or invoke it by `crate::macro_name` anywhere, before or after the definition.
    ///
    /// It is surprising that the macro will never be in the current module scope.
    /// These code fails with "unresolved import/macro",
    /// ```rust,compile_fail
    /// mod m { macro_rules! foo { () => {} } }
    /// use m::foo as bar;
    /// ```
    ///
    /// ```rust,compile_fail
    /// macro_rules! foo { () => {} }
    /// self::foo!();
    /// crate::foo!();
    /// ```
    ///
    /// Well, this code compiles, because the plain path `foo` in `use` is searched
    /// in the legacy textual scope only.
    /// ```rust
    /// macro_rules! foo { () => {} }
    /// use foo as bar;
    /// ```
    fn define_macro(
        &mut self,
        module_id: LocalModuleId,
        name: Name,
        macro_: MacroDefId,
        export: bool,
    ) {
        // Textual scoping
        self.define_legacy_macro(module_id, name.clone(), macro_);

        // Module scoping
        // In Rust, `#[macro_export]` macros are unconditionally visible at the
        // crate root, even if the parent modules is **not** visible.
        if export {
            self.update(
                self.def_map.root,
                None,
                &[(name, Resolution { def: PerNs::macros(macro_), import: None })],
            );
        }
    }

    /// Define a legacy textual scoped macro in module
    ///
    /// We use a map `legacy_macros` to store all legacy textual scoped macros visable per module.
    /// It will clone all macros from parent legacy scope, whose definition is prior to
    /// the definition of current module.
    /// And also, `macro_use` on a module will import all legacy macros visable inside to
    /// current legacy scope, with possible shadowing.
    fn define_legacy_macro(&mut self, module_id: LocalModuleId, name: Name, macro_: MacroDefId) {
        // Always shadowing
        self.def_map.modules[module_id].scope.legacy_macros.insert(name, macro_);
    }

    /// Import macros from `#[macro_use] extern crate`.
    fn import_macros_from_extern_crate(
        &mut self,
        current_module_id: LocalModuleId,
        import: &raw::ImportData,
    ) {
        log::debug!(
            "importing macros from extern crate: {:?} ({:?})",
            import,
            self.def_map.edition,
        );

        let res = self.def_map.resolve_name_in_extern_prelude(
            &import
                .path
                .as_ident()
                .expect("extern crate should have been desugared to one-element path"),
        );

        if let Some(ModuleDefId::ModuleId(m)) = res.take_types() {
            tested_by!(macro_rules_from_other_crates_are_visible_with_macro_use);
            self.import_all_macros_exported(current_module_id, m.krate);
        }
    }

    /// Import all exported macros from another crate
    ///
    /// Exported macros are just all macros in the root module scope.
    /// Note that it contains not only all `#[macro_export]` macros, but also all aliases
    /// created by `use` in the root module, ignoring the visibility of `use`.
    fn import_all_macros_exported(&mut self, current_module_id: LocalModuleId, krate: CrateId) {
        let def_map = self.db.crate_def_map(krate);
        for (name, def) in def_map[def_map.root].scope.macros() {
            // `macro_use` only bring things into legacy scope.
            self.define_legacy_macro(current_module_id, name.clone(), def);
        }
    }

    /// Import resolution
    ///
    /// This is a fix point algorithm. We resolve imports until no forward
    /// progress in resolving imports is made
    fn resolve_imports(&mut self) {
        let mut n_previous_unresolved = self.unresolved_imports.len() + 1;

        while self.unresolved_imports.len() < n_previous_unresolved {
            n_previous_unresolved = self.unresolved_imports.len();
            let imports = std::mem::replace(&mut self.unresolved_imports, Vec::new());
            for mut directive in imports {
                directive.status = self.resolve_import(directive.module_id, &directive.import);

                match directive.status {
                    PartialResolvedImport::Indeterminate(_) => {
                        self.record_resolved_import(&directive);
                        // FIXME: For avoid performance regression,
                        // we consider an imported resolved if it is indeterminate (i.e not all namespace resolved)
                        self.resolved_imports.push(directive)
                    }
                    PartialResolvedImport::Resolved(_) => {
                        self.record_resolved_import(&directive);
                        self.resolved_imports.push(directive)
                    }
                    PartialResolvedImport::Unresolved => {
                        self.unresolved_imports.push(directive);
                    }
                }
            }
        }
    }

    fn resolve_import(
        &self,
        module_id: LocalModuleId,
        import: &raw::ImportData,
    ) -> PartialResolvedImport {
        log::debug!("resolving import: {:?} ({:?})", import, self.def_map.edition);
        if import.is_extern_crate {
            let res = self.def_map.resolve_name_in_extern_prelude(
                &import
                    .path
                    .as_ident()
                    .expect("extern crate should have been desugared to one-element path"),
            );
            PartialResolvedImport::Resolved(res)
        } else {
            let res = self.def_map.resolve_path_fp_with_macro(
                self.db,
                ResolveMode::Import,
                module_id,
                &import.path,
                BuiltinShadowMode::Module,
            );

            let def = res.resolved_def;
            if res.reached_fixedpoint == ReachedFixedPoint::No {
                return PartialResolvedImport::Unresolved;
            }

            if let Some(krate) = res.krate {
                if krate != self.def_map.krate {
                    return PartialResolvedImport::Resolved(def);
                }
            }

            // Check whether all namespace is resolved
            if def.take_types().is_some()
                && def.take_values().is_some()
                && def.take_macros().is_some()
            {
                PartialResolvedImport::Resolved(def)
            } else {
                PartialResolvedImport::Indeterminate(def)
            }
        }
    }

    fn record_resolved_import(&mut self, directive: &ImportDirective) {
        let module_id = directive.module_id;
        let import_id = directive.import_id;
        let import = &directive.import;
        let def = directive.status.namespaces();

        if import.is_glob {
            log::debug!("glob import: {:?}", import);
            match def.take_types() {
                Some(ModuleDefId::ModuleId(m)) => {
                    if import.is_prelude {
                        tested_by!(std_prelude);
                        self.def_map.prelude = Some(m);
                    } else if m.krate != self.def_map.krate {
                        tested_by!(glob_across_crates);
                        // glob import from other crate => we can just import everything once
                        let item_map = self.db.crate_def_map(m.krate);
                        let scope = &item_map[m.local_id].scope;

                        // Module scoped macros is included
                        let items = scope
                            .items
                            .iter()
                            .map(|(name, res)| (name.clone(), res.clone()))
                            .collect::<Vec<_>>();

                        self.update(module_id, Some(import_id), &items);
                    } else {
                        // glob import from same crate => we do an initial
                        // import, and then need to propagate any further
                        // additions
                        let scope = &self.def_map[m.local_id].scope;

                        // Module scoped macros is included
                        let items = scope
                            .items
                            .iter()
                            .map(|(name, res)| (name.clone(), res.clone()))
                            .collect::<Vec<_>>();

                        self.update(module_id, Some(import_id), &items);
                        // record the glob import in case we add further items
                        let glob = self.glob_imports.entry(m.local_id).or_default();
                        if !glob.iter().any(|it| *it == (module_id, import_id)) {
                            glob.push((module_id, import_id));
                        }
                    }
                }
                Some(ModuleDefId::AdtId(AdtId::EnumId(e))) => {
                    tested_by!(glob_enum);
                    // glob import from enum => just import all the variants
                    let enum_data = self.db.enum_data(e);
                    let resolutions = enum_data
                        .variants
                        .iter()
                        .filter_map(|(local_id, variant_data)| {
                            let name = variant_data.name.clone();
                            let variant = EnumVariantId { parent: e, local_id };
                            let res = Resolution {
                                def: PerNs::both(variant.into(), variant.into()),
                                import: Some(import_id),
                            };
                            Some((name, res))
                        })
                        .collect::<Vec<_>>();
                    self.update(module_id, Some(import_id), &resolutions);
                }
                Some(d) => {
                    log::debug!("glob import {:?} from non-module/enum {:?}", import, d);
                }
                None => {
                    log::debug!("glob import {:?} didn't resolve as type", import);
                }
            }
        } else {
            match import.path.segments.last() {
                Some(last_segment) => {
                    let name = import.alias.clone().unwrap_or_else(|| last_segment.name.clone());
                    log::debug!("resolved import {:?} ({:?}) to {:?}", name, import, def);

                    // extern crates in the crate root are special-cased to insert entries into the extern prelude: rust-lang/rust#54658
                    if import.is_extern_crate && module_id == self.def_map.root {
                        if let Some(def) = def.take_types() {
                            self.def_map.extern_prelude.insert(name.clone(), def);
                        }
                    }

                    let resolution = Resolution { def, import: Some(import_id) };
                    self.update(module_id, Some(import_id), &[(name, resolution)]);
                }
                None => tested_by!(bogus_paths),
            }
        }
    }

    fn update(
        &mut self,
        module_id: LocalModuleId,
        import: Option<LocalImportId>,
        resolutions: &[(Name, Resolution)],
    ) {
        self.update_recursive(module_id, import, resolutions, 0)
    }

    fn update_recursive(
        &mut self,
        module_id: LocalModuleId,
        import: Option<LocalImportId>,
        resolutions: &[(Name, Resolution)],
        depth: usize,
    ) {
        if depth > 100 {
            // prevent stack overflows (but this shouldn't be possible)
            panic!("infinite recursion in glob imports!");
        }
        let module_items = &mut self.def_map.modules[module_id].scope;
        let mut changed = false;
        for (name, res) in resolutions {
            let existing = module_items.items.entry(name.clone()).or_default();

            if existing.def.types.is_none() && res.def.types.is_some() {
                existing.def.types = res.def.types;
                existing.import = import.or(res.import);
                changed = true;
            }
            if existing.def.values.is_none() && res.def.values.is_some() {
                existing.def.values = res.def.values;
                existing.import = import.or(res.import);
                changed = true;
            }
            if existing.def.macros.is_none() && res.def.macros.is_some() {
                existing.def.macros = res.def.macros;
                existing.import = import.or(res.import);
                changed = true;
            }

            if existing.def.is_none()
                && res.def.is_none()
                && existing.import.is_none()
                && res.import.is_some()
            {
                existing.import = res.import;
            }
        }

        if !changed {
            return;
        }
        let glob_imports = self
            .glob_imports
            .get(&module_id)
            .into_iter()
            .flat_map(|v| v.iter())
            .cloned()
            .collect::<Vec<_>>();
        for (glob_importing_module, glob_import) in glob_imports {
            // We pass the glob import so that the tracked import in those modules is that glob import
            self.update_recursive(glob_importing_module, Some(glob_import), resolutions, depth + 1);
        }
    }

    fn resolve_macros(&mut self) -> ReachedFixedPoint {
        let mut macros = std::mem::replace(&mut self.unexpanded_macros, Vec::new());
        let mut attribute_macros =
            std::mem::replace(&mut self.unexpanded_attribute_macros, Vec::new());
        let mut resolved = Vec::new();
        let mut res = ReachedFixedPoint::Yes;
        macros.retain(|directive| {
            if let Some(call_id) = directive.legacy {
                res = ReachedFixedPoint::No;
                resolved.push((directive.module_id, call_id));
                return false;
            }

            let resolved_res = self.def_map.resolve_path_fp_with_macro(
                self.db,
                ResolveMode::Other,
                directive.module_id,
                &directive.path,
                BuiltinShadowMode::Module,
            );

            if let Some(def) = resolved_res.resolved_def.take_macros() {
                let call_id = def.as_call_id(self.db, MacroCallKind::FnLike(directive.ast_id));
                resolved.push((directive.module_id, call_id));
                res = ReachedFixedPoint::No;
                return false;
            }

            true
        });
        attribute_macros.retain(|(module_id, ast_id, path)| {
            let resolved_res = self.resolve_attribute_macro(path);

            if let Some(def) = resolved_res {
                let call_id = def.as_call_id(self.db, MacroCallKind::Attr(*ast_id));
                resolved.push((*module_id, call_id));
                res = ReachedFixedPoint::No;
                return false;
            }

            true
        });

        self.unexpanded_macros = macros;
        self.unexpanded_attribute_macros = attribute_macros;

        for (module_id, macro_call_id) in resolved {
            self.collect_macro_expansion(module_id, macro_call_id);
        }

        res
    }

    fn resolve_attribute_macro(&self, path: &Path) -> Option<MacroDefId> {
        // FIXME this is currently super hacky, just enough to support the
        // built-in derives
        if let Some(name) = path.as_ident() {
            // FIXME this should actually be handled with the normal name
            // resolution; the std lib defines built-in stubs for the derives,
            // but these are new-style `macro`s, which we don't support yet
            if let Some(def_id) = find_builtin_derive(name) {
                return Some(def_id);
            }
        }
        None
    }

    fn collect_macro_expansion(&mut self, module_id: LocalModuleId, macro_call_id: MacroCallId) {
        let file_id: HirFileId = macro_call_id.as_file();
        let raw_items = self.db.raw_items(file_id);
        let mod_dir = self.mod_dirs[&module_id].clone();
        ModCollector {
            def_collector: &mut *self,
            file_id,
            module_id,
            raw_items: &raw_items,
            mod_dir,
        }
        .collect(raw_items.items());
    }

    fn finish(self) -> CrateDefMap {
        self.def_map
    }
}

/// Walks a single module, populating defs, imports and macros
struct ModCollector<'a, D> {
    def_collector: D,
    module_id: LocalModuleId,
    file_id: HirFileId,
    raw_items: &'a raw::RawItems,
    mod_dir: ModDir,
}

impl<DB> ModCollector<'_, &'_ mut DefCollector<'_, DB>>
where
    DB: DefDatabase,
{
    fn collect(&mut self, items: &[raw::RawItem]) {
        // Note: don't assert that inserted value is fresh: it's simply not true
        // for macros.
        self.def_collector.mod_dirs.insert(self.module_id, self.mod_dir.clone());

        // Prelude module is always considered to be `#[macro_use]`.
        if let Some(prelude_module) = self.def_collector.def_map.prelude {
            if prelude_module.krate != self.def_collector.def_map.krate {
                tested_by!(prelude_is_macro_use);
                self.def_collector.import_all_macros_exported(self.module_id, prelude_module.krate);
            }
        }

        // This should be processed eagerly instead of deferred to resolving.
        // `#[macro_use] extern crate` is hoisted to imports macros before collecting
        // any other items.
        for item in items {
            if self.is_cfg_enabled(&item.attrs) {
                if let raw::RawItemKind::Import(import_id) = item.kind {
                    let import = self.raw_items[import_id].clone();
                    if import.is_extern_crate && import.is_macro_use {
                        self.def_collector.import_macros_from_extern_crate(self.module_id, &import);
                    }
                }
            }
        }

        for item in items {
            if self.is_cfg_enabled(&item.attrs) {
                match item.kind {
                    raw::RawItemKind::Module(m) => {
                        self.collect_module(&self.raw_items[m], &item.attrs)
                    }
                    raw::RawItemKind::Import(import_id) => {
                        self.def_collector.unresolved_imports.push(ImportDirective {
                            module_id: self.module_id,
                            import_id,
                            import: self.raw_items[import_id].clone(),
                            status: PartialResolvedImport::Unresolved,
                        })
                    }
                    raw::RawItemKind::Def(def) => {
                        self.define_def(&self.raw_items[def], &item.attrs)
                    }
                    raw::RawItemKind::Macro(mac) => self.collect_macro(&self.raw_items[mac]),
                    raw::RawItemKind::Impl(imp) => {
                        let module = ModuleId {
                            krate: self.def_collector.def_map.krate,
                            local_id: self.module_id,
                        };
                        let ctx = LocationCtx::new(self.def_collector.db, module, self.file_id);
                        let imp_id = ImplId::from_ast_id(ctx, self.raw_items[imp].ast_id);
                        self.def_collector.def_map.modules[self.module_id].impls.push(imp_id)
                    }
                }
            }
        }
    }

    fn collect_module(&mut self, module: &raw::ModuleData, attrs: &Attrs) {
        let path_attr = attrs.by_key("path").string_value();
        let is_macro_use = attrs.by_key("macro_use").exists();
        match module {
            // inline module, just recurse
            raw::ModuleData::Definition { name, items, ast_id } => {
                let module_id =
                    self.push_child_module(name.clone(), AstId::new(self.file_id, *ast_id), None);

                ModCollector {
                    def_collector: &mut *self.def_collector,
                    module_id,
                    file_id: self.file_id,
                    raw_items: self.raw_items,
                    mod_dir: self.mod_dir.descend_into_definition(name, path_attr),
                }
                .collect(&*items);
                if is_macro_use {
                    self.import_all_legacy_macros(module_id);
                }
            }
            // out of line module, resolve, parse and recurse
            raw::ModuleData::Declaration { name, ast_id } => {
                let ast_id = AstId::new(self.file_id, *ast_id);
                match self.mod_dir.resolve_declaration(
                    self.def_collector.db,
                    self.file_id,
                    name,
                    path_attr,
                ) {
                    Ok((file_id, mod_dir)) => {
                        let module_id = self.push_child_module(name.clone(), ast_id, Some(file_id));
                        let raw_items = self.def_collector.db.raw_items(file_id.into());
                        ModCollector {
                            def_collector: &mut *self.def_collector,
                            module_id,
                            file_id: file_id.into(),
                            raw_items: &raw_items,
                            mod_dir,
                        }
                        .collect(raw_items.items());
                        if is_macro_use {
                            self.import_all_legacy_macros(module_id);
                        }
                    }
                    Err(candidate) => self.def_collector.def_map.diagnostics.push(
                        DefDiagnostic::UnresolvedModule {
                            module: self.module_id,
                            declaration: ast_id,
                            candidate,
                        },
                    ),
                };
            }
        }
    }

    fn push_child_module(
        &mut self,
        name: Name,
        declaration: AstId<ast::Module>,
        definition: Option<FileId>,
    ) -> LocalModuleId {
        let modules = &mut self.def_collector.def_map.modules;
        let res = modules.alloc(ModuleData::default());
        modules[res].parent = Some(self.module_id);
        modules[res].origin = ModuleOrigin::not_sure_file(definition, declaration);
        modules[res].scope.legacy_macros = modules[self.module_id].scope.legacy_macros.clone();
        modules[self.module_id].children.insert(name.clone(), res);
        let resolution = Resolution {
            def: PerNs::types(
                ModuleId { krate: self.def_collector.def_map.krate, local_id: res }.into(),
            ),
            import: None,
        };
        self.def_collector.update(self.module_id, None, &[(name, resolution)]);
        res
    }

    fn define_def(&mut self, def: &raw::DefData, attrs: &Attrs) {
        let module = ModuleId { krate: self.def_collector.def_map.krate, local_id: self.module_id };
        let ctx = LocationCtx::new(self.def_collector.db, module, self.file_id);

        // FIXME: check attrs to see if this is an attribute macro invocation;
        // in which case we don't add the invocation, just a single attribute
        // macro invocation

        self.collect_derives(attrs, def);

        let name = def.name.clone();
        let def: PerNs = match def.kind {
            raw::DefKind::Function(ast_id) => {
                let def = FunctionLoc {
                    container: ContainerId::ModuleId(module),
                    ast_id: AstId::new(self.file_id, ast_id),
                }
                .intern(self.def_collector.db);

                PerNs::values(def.into())
            }
            raw::DefKind::Struct(ast_id) => {
                let id = StructId::from_ast_id(ctx, ast_id).into();
                PerNs::both(id, id)
            }
            raw::DefKind::Union(ast_id) => {
                let id = UnionId::from_ast_id(ctx, ast_id).into();
                PerNs::both(id, id)
            }
            raw::DefKind::Enum(ast_id) => PerNs::types(EnumId::from_ast_id(ctx, ast_id).into()),
            raw::DefKind::Const(ast_id) => {
                let def = ConstLoc {
                    container: ContainerId::ModuleId(module),
                    ast_id: AstId::new(self.file_id, ast_id),
                }
                .intern(self.def_collector.db);

                PerNs::values(def.into())
            }
            raw::DefKind::Static(ast_id) => {
                let def = StaticLoc { container: module, ast_id: AstId::new(self.file_id, ast_id) }
                    .intern(self.def_collector.db);

                PerNs::values(def.into())
            }
            raw::DefKind::Trait(ast_id) => PerNs::types(TraitId::from_ast_id(ctx, ast_id).into()),
            raw::DefKind::TypeAlias(ast_id) => {
                let def = TypeAliasLoc {
                    container: ContainerId::ModuleId(module),
                    ast_id: AstId::new(self.file_id, ast_id),
                }
                .intern(self.def_collector.db);

                PerNs::types(def.into())
            }
        };
        let resolution = Resolution { def, import: None };
        self.def_collector.update(self.module_id, None, &[(name, resolution)])
    }

    fn collect_derives(&mut self, attrs: &Attrs, def: &raw::DefData) {
        for derive_subtree in attrs.by_key("derive").tt_values() {
            // for #[derive(Copy, Clone)], `derive_subtree` is the `(Copy, Clone)` subtree
            for tt in &derive_subtree.token_trees {
                let ident = match &tt {
                    tt::TokenTree::Leaf(tt::Leaf::Ident(ident)) => ident,
                    tt::TokenTree::Leaf(tt::Leaf::Punct(_)) => continue, // , is ok
                    _ => continue, // anything else would be an error (which we currently ignore)
                };
                let path = Path::from_tt_ident(ident);

                let ast_id = AstId::new(self.file_id, def.kind.ast_id());
                self.def_collector.unexpanded_attribute_macros.push((self.module_id, ast_id, path));
            }
        }
    }

    fn collect_macro(&mut self, mac: &raw::MacroData) {
        let ast_id = AstId::new(self.file_id, mac.ast_id);

        // Case 0: builtin macros
        if mac.builtin {
            if let Some(name) = &mac.name {
                let krate = self.def_collector.def_map.krate;
                if let Some(macro_id) = find_builtin_macro(name, krate, ast_id) {
                    self.def_collector.define_macro(
                        self.module_id,
                        name.clone(),
                        macro_id,
                        mac.export,
                    );
                    return;
                }
            }
        }

        // Case 1: macro rules, define a macro in crate-global mutable scope
        if is_macro_rules(&mac.path) {
            if let Some(name) = &mac.name {
                let macro_id = MacroDefId {
                    ast_id: Some(ast_id),
                    krate: Some(self.def_collector.def_map.krate),
                    kind: MacroDefKind::Declarative,
                };
                self.def_collector.define_macro(self.module_id, name.clone(), macro_id, mac.export);
            }
            return;
        }

        // Case 2: try to resolve in legacy scope and expand macro_rules
        if let Some(macro_def) = mac.path.as_ident().and_then(|name| {
            self.def_collector.def_map[self.module_id].scope.get_legacy_macro(&name)
        }) {
            let macro_call_id =
                macro_def.as_call_id(self.def_collector.db, MacroCallKind::FnLike(ast_id));

            self.def_collector.unexpanded_macros.push(MacroDirective {
                module_id: self.module_id,
                path: mac.path.clone(),
                ast_id,
                legacy: Some(macro_call_id),
            });

            return;
        }

        // Case 3: resolve in module scope, expand during name resolution.
        // We rewrite simple path `macro_name` to `self::macro_name` to force resolve in module scope only.
        let mut path = mac.path.clone();
        if path.is_ident() {
            path.kind = PathKind::Self_;
        }

        self.def_collector.unexpanded_macros.push(MacroDirective {
            module_id: self.module_id,
            path,
            ast_id,
            legacy: None,
        });
    }

    fn import_all_legacy_macros(&mut self, module_id: LocalModuleId) {
        let macros = self.def_collector.def_map[module_id].scope.legacy_macros.clone();
        for (name, macro_) in macros {
            self.def_collector.define_legacy_macro(self.module_id, name.clone(), macro_);
        }
    }

    fn is_cfg_enabled(&self, attrs: &Attrs) -> bool {
        // FIXME: handle cfg_attr :-)
        attrs
            .by_key("cfg")
            .tt_values()
            .all(|tt| self.def_collector.cfg_options.is_cfg_enabled(tt) != Some(false))
    }
}

fn is_macro_rules(path: &Path) -> bool {
    path.as_ident() == Some(&name::MACRO_RULES)
}

#[cfg(test)]
mod tests {
    use crate::{db::DefDatabase, test_db::TestDB};
    use ra_arena::Arena;
    use ra_db::{fixture::WithFixture, SourceDatabase};

    use super::*;

    fn do_collect_defs(db: &impl DefDatabase, def_map: CrateDefMap) -> CrateDefMap {
        let mut collector = DefCollector {
            db,
            def_map,
            glob_imports: FxHashMap::default(),
            unresolved_imports: Vec::new(),
            resolved_imports: Vec::new(),
            unexpanded_macros: Vec::new(),
            unexpanded_attribute_macros: Vec::new(),
            mod_dirs: FxHashMap::default(),
            cfg_options: &CfgOptions::default(),
        };
        collector.collect();
        collector.def_map
    }

    fn do_resolve(code: &str) -> CrateDefMap {
        let (db, _file_id) = TestDB::with_single_file(&code);
        let krate = db.test_crate();

        let def_map = {
            let edition = db.crate_graph().edition(krate);
            let mut modules: Arena<LocalModuleId, ModuleData> = Arena::default();
            let root = modules.alloc(ModuleData::default());
            CrateDefMap {
                krate,
                edition,
                extern_prelude: FxHashMap::default(),
                prelude: None,
                root,
                modules,
                diagnostics: Vec::new(),
            }
        };
        do_collect_defs(&db, def_map)
    }

    #[test]
    fn test_macro_expand_will_stop() {
        do_resolve(
            r#"
        macro_rules! foo {
            ($($ty:ty)*) => { foo!($($ty)*, $($ty)*); }
        }
foo!(KABOOM);
        "#,
        );
    }
}
