use deis_build_core::routes::HandlerKind;
use deis_build_core::{
    HandlerInfo, ModuleReference, ModuleReferenceKind, ModuleSourcePosition,
    NonPortableStreamConstructorReference, NonStandardContextReference,
    ProviderOnlyGlobalReference, SpecifierLiteral,
};
use std::collections::HashSet;
use swc_common::{
    sync::Lrc, BytePos, FileName, Globals, Mark, SourceMap, Span, Spanned, SyntaxContext, GLOBALS,
};
use swc_ecma_ast::*;
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::{Visit, VisitMutWith, VisitWith};

pub struct TypeScriptParser {
    source_map: Lrc<SourceMap>,
}

impl Default for TypeScriptParser {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeScriptParser {
    pub fn new() -> Self {
        Self {
            source_map: Lrc::new(SourceMap::default()),
        }
    }

    /// Parse a module as an HTTP handler, or as a module that is not a
    /// handler at all (whose context references are then never looked at).
    pub fn parse(&self, code: &str, filename: &str) -> anyhow::Result<HandlerInfo> {
        self.parse_handler(code, filename, HandlerKind::Http)
    }

    /// Parse a handler of the given kind.
    ///
    /// The kind decides which parameter of the default export is the
    /// platform's context ([`HandlerKind::context_parameter`]) and which of the
    /// fields read off it are recorded as refused
    /// ([`HandlerKind::refuses_context_field`]).
    pub fn parse_handler(
        &self,
        code: &str,
        filename: &str,
        kind: HandlerKind,
    ) -> anyhow::Result<HandlerInfo> {
        let source_file = self.source_map.new_source_file(
            FileName::Custom(filename.to_string()).into(),
            code.to_string(),
        );
        let file_start = source_file.start_pos;

        let lexer = Lexer::new(
            Syntax::Typescript(Default::default()),
            EsVersion::Es2022,
            StringInput::from(&*source_file),
            None,
        );

        let mut parser = Parser::new_from(lexer);
        let mut module = parser
            .parse_module()
            .map_err(|e| anyhow::anyhow!("Parse error: {e:?}"))?;

        let visitor = GLOBALS.set(&Globals::new(), || {
            // Give unresolved globals a stable syntax context before the
            // policy visitor runs. A spelling such as `WebSocket` can also be
            // a perfectly portable local class or import; only the unresolved
            // global is the provider API that EC-STREAM excludes.
            let unresolved_mark = Mark::new();
            let top_level_mark = Mark::new();
            module.visit_mut_with(&mut resolver(unresolved_mark, top_level_mark, true));
            let unresolved_context = SyntaxContext::empty().apply_mark(unresolved_mark);
            let mut exported = DefaultHandlerCollector::default();
            module.visit_with(&mut exported);
            loop {
                let before = exported.callables.len();
                let mut aliases = DefaultHandlerAliasCollector::new(&exported.callables);
                module.visit_with(&mut aliases);
                exported.callables.extend(aliases.sources);
                if exported.callables.len() == before {
                    break;
                }
            }
            let mut contexts =
                DefaultHandlerContextCollector::new(exported.callables, kind.context_parameter());
            module.visit_with(&mut contexts);
            loop {
                let before = contexts.contexts.len();
                let mut aliases = ContextAliasCollector::new(&contexts.contexts);
                module.visit_with(&mut aliases);
                contexts.contexts.extend(aliases.aliases);
                if contexts.contexts.len() == before {
                    break;
                }
            }
            let DefaultHandlerContextCollector {
                contexts,
                destructured_fields,
                ..
            } = contexts;
            let mut visitor = HandlerVisitor::new(
                self.source_map.clone(),
                file_start,
                unresolved_context,
                contexts,
                kind,
            );
            for (field, span) in destructured_fields {
                visitor.record_context_field(field, span);
            }
            module.visit_with(&mut visitor);
            visitor
        });

        Ok(HandlerInfo {
            path: filename.to_string(),
            route: Self::path_to_route(filename),
            has_default_export: visitor.has_default_export,
            named_exports: visitor.named_exports,
            imports: visitor.imports,
            specifier_literals: visitor.specifier_literals,
            module_references: visitor.module_references,
            non_literal_module_loads: visitor.non_literal_module_loads,
            non_literal_module_load_positions: visitor.non_literal_module_load_positions,
            provider_only_global_references: visitor.provider_only_global_references,
            non_portable_stream_constructors: visitor.non_portable_stream_constructors,
            non_standard_context_references: visitor.non_standard_context_references,
        })
    }

    /// A rough route for the handler being parsed, used only in diagnostics.
    ///
    /// Each affix comes off once. `trim_start_matches` / `trim_end_matches`
    /// take theirs off as many times as they appear, so
    /// `functions/functions/x.ts` lost both directories and `report.ts.ts` lost
    /// both suffixes - the same repeated-prefix mistake that made a file under
    /// a directory called `src` into the application's queue consumer.
    ///
    /// The route the build actually uses is `deis_build_core::routes`, which
    /// this deliberately does not try to reproduce: it knows nothing about
    /// `[id]` or `[[catch]]`, and a second definition of the routing rule is
    /// how an app ends up answering on one target and 404ing on another.
    fn path_to_route(path: &str) -> String {
        // Simple route extraction
        // functions/api/users.ts -> /api/users
        // functions/index.ts -> /
        let route = path.strip_prefix("functions/").unwrap_or(path);
        let route = route
            .strip_suffix(".ts")
            .or_else(|| route.strip_suffix(".js"))
            .unwrap_or(route);

        if route == "index" || route.is_empty() {
            "/".to_string()
        } else {
            format!("/{route}")
        }
    }
}

#[derive(Default)]
struct DefaultHandlerCollector {
    callables: HashSet<Id>,
}

impl Visit for DefaultHandlerCollector {
    fn visit_export_default_expr(&mut self, expression: &ExportDefaultExpr) {
        if let Expr::Ident(ident) = &*expression.expr {
            self.callables.insert(ident.to_id());
        }
        expression.visit_children_with(self);
    }

    fn visit_named_export(&mut self, export: &NamedExport) {
        for specifier in &export.specifiers {
            if let ExportSpecifier::Named(named) = specifier {
                let exported = named.exported.as_ref().unwrap_or(&named.orig);
                if export_name(exported) == "default" {
                    if let ModuleExportName::Ident(local) = &named.orig {
                        self.callables.insert(local.to_id());
                    }
                }
            }
        }
        export.visit_children_with(self);
    }
}

/// Follow local aliases backwards from the exported value to the function
/// expression that implements it. The resolver has already given every
/// binding a distinct `Id`, so a same-spelled variable in another scope cannot
/// be mistaken for the exported handler.
struct DefaultHandlerAliasCollector<'a> {
    callables: &'a HashSet<Id>,
    sources: HashSet<Id>,
}

impl<'a> DefaultHandlerAliasCollector<'a> {
    fn new(callables: &'a HashSet<Id>) -> Self {
        Self {
            callables,
            sources: HashSet::new(),
        }
    }
}

impl Visit for DefaultHandlerAliasCollector<'_> {
    fn visit_var_declarator(&mut self, declaration: &VarDeclarator) {
        if let (Pat::Ident(alias), Some(Expr::Ident(source))) =
            (&declaration.name, declaration.init.as_deref())
        {
            if self.callables.contains(&alias.id.to_id()) {
                self.sources.insert(source.to_id());
            }
        }
        declaration.visit_children_with(self);
    }
}

struct DefaultHandlerContextCollector {
    callables: HashSet<Id>,
    /// Which parameter is the context; see [`HandlerKind::context_parameter`].
    context_parameter: usize,
    contexts: HashSet<Id>,
    destructured_fields: Vec<(String, Span)>,
}

impl DefaultHandlerContextCollector {
    fn new(callables: HashSet<Id>, context_parameter: usize) -> Self {
        Self {
            callables,
            context_parameter,
            contexts: HashSet::new(),
            destructured_fields: Vec::new(),
        }
    }

    fn record_context_parameter(&mut self, parameters: &[Param]) {
        if let Some(parameter) = parameters.get(self.context_parameter) {
            self.record_context_pattern(&parameter.pat);
        }
    }

    fn record_context_parameter_pattern(&mut self, parameters: &[Pat]) {
        if let Some(parameter) = parameters.get(self.context_parameter) {
            self.record_context_pattern(parameter);
        }
    }

    fn record_context_pattern(&mut self, pattern: &Pat) {
        match pattern {
            Pat::Ident(binding) => {
                self.contexts.insert(binding.id.to_id());
            }
            Pat::Object(object) => {
                for property in &object.props {
                    match property {
                        ObjectPatProp::KeyValue(key_value) => {
                            self.record_context_property(&key_value.key)
                        }
                        ObjectPatProp::Assign(assign) => self
                            .destructured_fields
                            .push((assign.key.id.sym.to_string(), assign.key.id.span)),
                        // Rest observes only fields that exist on the exact
                        // four-field object and therefore does not request a
                        // provider extension.
                        ObjectPatProp::Rest(_) => {}
                    }
                }
            }
            Pat::Assign(assign) => self.record_context_pattern(&assign.left),
            _ => {}
        }
    }

    fn record_context_property(&mut self, property: &PropName) {
        let field = match property {
            PropName::Ident(field) => (field.sym.to_string(), field.span),
            PropName::Str(field) => (field.value.to_string_lossy().into_owned(), field.span),
            PropName::Computed(computed) => match &*computed.expr {
                Expr::Lit(Lit::Str(field)) => {
                    (field.value.to_string_lossy().into_owned(), field.span)
                }
                _ => ("<dynamic>".to_string(), computed.span),
            },
            PropName::Num(number) => (number.value.to_string(), number.span),
            PropName::BigInt(number) => (number.value.to_string(), number.span),
        };
        if !self.destructured_fields.contains(&field) {
            self.destructured_fields.push(field);
        }
    }
}

impl Visit for DefaultHandlerContextCollector {
    fn visit_export_default_decl(&mut self, declaration: &ExportDefaultDecl) {
        if let DefaultDecl::Fn(function) = &declaration.decl {
            self.record_context_parameter(&function.function.params);
        }
        declaration.visit_children_with(self);
    }

    fn visit_export_default_expr(&mut self, expression: &ExportDefaultExpr) {
        match &*expression.expr {
            Expr::Fn(function) => self.record_context_parameter(&function.function.params),
            Expr::Arrow(arrow) => self.record_context_parameter_pattern(&arrow.params),
            _ => {}
        }
        expression.visit_children_with(self);
    }

    fn visit_fn_decl(&mut self, declaration: &FnDecl) {
        if self.callables.contains(&declaration.ident.to_id()) {
            self.record_context_parameter(&declaration.function.params);
        }
        declaration.visit_children_with(self);
    }

    fn visit_var_declarator(&mut self, declaration: &VarDeclarator) {
        let Pat::Ident(binding) = &declaration.name else {
            declaration.visit_children_with(self);
            return;
        };
        if !self.callables.contains(&binding.id.to_id()) {
            declaration.visit_children_with(self);
            return;
        }
        match declaration.init.as_deref() {
            Some(Expr::Fn(function)) => self.record_context_parameter(&function.function.params),
            Some(Expr::Arrow(arrow)) => self.record_context_parameter_pattern(&arrow.params),
            _ => {}
        }
        declaration.visit_children_with(self);
    }
}

struct ContextAliasCollector<'a> {
    contexts: &'a HashSet<Id>,
    aliases: HashSet<Id>,
}

impl<'a> ContextAliasCollector<'a> {
    fn new(contexts: &'a HashSet<Id>) -> Self {
        Self {
            contexts,
            aliases: HashSet::new(),
        }
    }
}

impl Visit for ContextAliasCollector<'_> {
    fn visit_var_declarator(&mut self, declaration: &VarDeclarator) {
        if let (Pat::Ident(alias), Some(Expr::Ident(source))) =
            (&declaration.name, declaration.init.as_deref())
        {
            if self.contexts.contains(&source.to_id()) {
                self.aliases.insert(alias.id.to_id());
            }
        }
        declaration.visit_children_with(self);
    }
}

struct HandlerVisitor {
    source_map: Lrc<SourceMap>,
    /// Where this file begins in the shared source map. One parser instance
    /// keeps one map across every file it is handed, so a span's `BytePos` is
    /// an offset into that whole map and not into the file. Subtracting this
    /// is what turns a span back into an index the caller can slice its own
    /// source string with.
    file_start: BytePos,
    unresolved_context: SyntaxContext,
    has_default_export: bool,
    named_exports: Vec<String>,
    imports: Vec<String>,
    specifier_literals: Vec<SpecifierLiteral>,
    module_references: Vec<ModuleReference>,
    non_literal_module_loads: usize,
    non_literal_module_load_positions: Vec<ModuleSourcePosition>,
    provider_only_global_references: Vec<ProviderOnlyGlobalReference>,
    non_portable_stream_constructors: Vec<NonPortableStreamConstructorReference>,
    context_bindings: HashSet<Id>,
    kind: HandlerKind,
    non_standard_context_references: Vec<NonStandardContextReference>,
}

impl HandlerVisitor {
    fn new(
        source_map: Lrc<SourceMap>,
        file_start: BytePos,
        unresolved_context: SyntaxContext,
        context_bindings: HashSet<Id>,
        kind: HandlerKind,
    ) -> Self {
        Self {
            source_map,
            file_start,
            unresolved_context,
            has_default_export: false,
            named_exports: Vec::new(),
            imports: Vec::new(),
            specifier_literals: Vec::new(),
            module_references: Vec::new(),
            non_literal_module_loads: 0,
            non_literal_module_load_positions: Vec::new(),
            provider_only_global_references: Vec::new(),
            non_portable_stream_constructors: Vec::new(),
            context_bindings,
            kind,
            non_standard_context_references: Vec::new(),
        }
    }

    fn static_context_field(&self, member: &MemberExpr) -> Option<(String, Span)> {
        let Expr::Ident(context) = &*member.obj else {
            return None;
        };
        if !self.context_bindings.contains(&context.to_id()) {
            return None;
        }
        match &member.prop {
            MemberProp::Ident(field) => Some((field.sym.to_string(), field.span)),
            MemberProp::Computed(computed) => match &*computed.expr {
                Expr::Lit(Lit::Str(field)) => {
                    Some((field.value.to_string_lossy().into_owned(), field.span))
                }
                _ => Some(("<dynamic>".to_string(), computed.span)),
            },
            MemberProp::PrivateName(field) => Some((field.name.to_string(), field.span)),
        }
    }

    fn record_context_field(&mut self, field: String, span: Span) {
        if matches!(field.as_str(), "request" | "env" | "params" | "waitUntil")
            || !self.kind.refuses_context_field(&field)
        {
            return;
        }
        let location = self.source_map.lookup_char_pos(span.lo());
        let reference = NonStandardContextReference {
            field,
            line: location.line,
            column: location.col_display + 1,
        };
        if !self
            .non_standard_context_references
            .iter()
            .any(|existing| existing == &reference)
        {
            self.non_standard_context_references.push(reference);
        }
    }

    /// Note that this file names `specifier`, and where the literal that
    /// names it sits.
    ///
    /// `imports` is deduplicated because staging only needs to know which
    /// files to walk. `specifier_literals` deliberately is not: rewriting a
    /// misspelled import has to reach every occurrence, and two occurrences of
    /// the same text are two different byte ranges.
    fn record_import(&mut self, specifier: &str, literal: Span) {
        if !self.imports.iter().any(|existing| existing == specifier) {
            self.imports.push(specifier.to_string());
        }
        self.specifier_literals.push(SpecifierLiteral {
            specifier: specifier.to_string(),
            start: (literal.lo() - self.file_start).0 as usize,
            end: (literal.hi() - self.file_start).0 as usize,
        });
    }

    fn record_module_reference(
        &mut self,
        specifier: &str,
        requested_exports: Vec<String>,
        loads_namespace: bool,
        kind: ModuleReferenceKind,
        span: Span,
    ) {
        let location = self.source_map.lookup_char_pos(span.lo());
        self.module_references.push(ModuleReference {
            specifier: specifier.to_string(),
            requested_exports,
            loads_namespace,
            kind,
            line: location.line,
            column: location.col_display + 1,
        });
    }

    fn record_provider_only_global(&mut self, name: &str, span: Span) {
        if !deis_build_core::PROVIDER_ONLY_GLOBALS.contains(&name) {
            return;
        }
        let location = self.source_map.lookup_char_pos(span.lo());
        let reference = ProviderOnlyGlobalReference {
            name: name.to_string(),
            line: location.line,
            column: location.col_display + 1,
        };
        if !self
            .provider_only_global_references
            .iter()
            .any(|existing| existing == &reference)
        {
            self.provider_only_global_references.push(reference);
        }
    }

    fn is_global_object(&self, expression: &Expr) -> bool {
        match expression {
            Expr::Ident(ident) => {
                matches!(ident.sym.as_ref(), "globalThis" | "self")
                    && ident.ctxt == self.unresolved_context
            }
            Expr::Paren(paren) => self.is_global_object(&paren.expr),
            _ => false,
        }
    }

    fn record_provider_only_property_name(&mut self, property: &PropName) {
        match property {
            PropName::Ident(ident) => {
                self.record_provider_only_global(ident.sym.as_ref(), ident.span)
            }
            PropName::Str(value) => {
                self.record_provider_only_global(&value.value.to_string_lossy(), value.span)
            }
            PropName::Computed(computed) => {
                if let Expr::Lit(Lit::Str(value)) = &*computed.expr {
                    self.record_provider_only_global(&value.value.to_string_lossy(), value.span);
                }
            }
            PropName::Num(_) | PropName::BigInt(_) => {}
        }
    }

    /// Return the statically named application global used by an expression.
    /// Resolver context distinguishes a true runtime global from a local or
    /// imported binding with the same spelling. Parenthesized and TypeScript-
    /// only wrappers do not change the runtime callee.
    fn static_global_name(&self, expression: &Expr) -> Option<(String, Span)> {
        match expression {
            Expr::Ident(ident) if ident.ctxt == self.unresolved_context => {
                Some((ident.sym.to_string(), ident.span))
            }
            Expr::Member(member) if self.is_global_object(&member.obj) => match &member.prop {
                MemberProp::Ident(ident) => Some((ident.sym.to_string(), ident.span)),
                MemberProp::Computed(computed) => match &*computed.expr {
                    Expr::Lit(Lit::Str(value)) => {
                        Some((value.value.to_string_lossy().into_owned(), value.span))
                    }
                    _ => None,
                },
                MemberProp::PrivateName(_) => None,
            },
            Expr::Paren(value) => self.static_global_name(&value.expr),
            Expr::TsAs(value) => self.static_global_name(&value.expr),
            Expr::TsTypeAssertion(value) => self.static_global_name(&value.expr),
            Expr::TsNonNull(value) => self.static_global_name(&value.expr),
            Expr::TsInstantiation(value) => self.static_global_name(&value.expr),
            Expr::TsSatisfies(value) => self.static_global_name(&value.expr),
            _ => None,
        }
    }

    fn record_non_portable_stream_constructor(&mut self, expression: &NewExpr) {
        let Some((name, span)) = self.static_global_name(&expression.callee) else {
            return;
        };
        let direct = matches!(name.as_str(), "ReadableStream" | "WritableStream");
        let configured_transform = name == "TransformStream"
            && expression
                .args
                .as_ref()
                .is_some_and(|arguments| !arguments.is_empty());
        if !direct && !configured_transform {
            return;
        }
        let location = self.source_map.lookup_char_pos(span.lo());
        let reference = NonPortableStreamConstructorReference {
            name,
            line: location.line,
            column: location.col_display + 1,
        };
        if !self
            .non_portable_stream_constructors
            .iter()
            .any(|existing| existing == &reference)
        {
            self.non_portable_stream_constructors.push(reference);
        }
    }
}

/// The text of an exported name, whether it was written as an identifier or as
/// a string literal (`export { x as "with-a-dash" }`).
fn export_name(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::Ident(ident) => ident.sym.to_string(),
        ModuleExportName::Str(literal) => literal.value.to_string_lossy().into_owned(),
    }
}

impl Visit for HandlerVisitor {
    fn visit_export_default_decl(&mut self, declaration: &ExportDefaultDecl) {
        self.has_default_export = true;
        declaration.visit_children_with(self);
    }

    fn visit_export_default_expr(&mut self, expression: &ExportDefaultExpr) {
        self.has_default_export = true;
        expression.visit_children_with(self);
    }

    fn visit_export_decl(&mut self, n: &ExportDecl) {
        // Handle: export const/let/function/class
        match &n.decl {
            Decl::Var(var_decl) => {
                for decl in var_decl.decls.iter() {
                    if let Pat::Ident(ident) = &decl.name {
                        self.named_exports.push(ident.id.sym.to_string());
                    }
                }
            }
            Decl::Fn(fn_decl) => {
                self.named_exports.push(fn_decl.ident.sym.to_string());
            }
            Decl::Class(class_decl) => {
                self.named_exports.push(class_decl.ident.sym.to_string());
            }
            _ => {}
        }
        n.visit_children_with(self);
    }

    fn visit_named_export(&mut self, n: &NamedExport) {
        // `export { foo, bar }`, `export { handler as default }`,
        // `export { default } from "./elsewhere"`.
        //
        // The name that matters is the one the module is exported *under*, not
        // the local binding: `export { handler as default }` is a default
        // export, and this used to record it as a named export called
        // `handler` with `has_default_export` still false. That was invisible
        // while a missing default export was not an error; the moment it
        // became one it would have failed a build over an idiomatic file that
        // is perfectly correct. `export { default as thing }` is the mirror
        // image and has to come out the other way round: a named export called
        // `thing`, and no default.
        for spec in n.specifiers.iter() {
            let name = match spec {
                ExportSpecifier::Named(named) => {
                    export_name(named.exported.as_ref().unwrap_or(&named.orig))
                }
                // `export * as ns from "./x"` publishes one name and no
                // default; `export * from "./x"` publishes whatever the other
                // module does, which this file cannot know.
                ExportSpecifier::Namespace(namespace) => export_name(&namespace.name),
                ExportSpecifier::Default(_) => "default".to_string(),
            };
            if name == "default" {
                self.has_default_export = true;
            } else {
                self.named_exports.push(name);
            }
        }
        if let Some(source) = &n.src {
            let literal = source.span;
            let source = source.value.to_string_lossy();
            self.record_import(&source, literal);
            if !n.type_only {
                let mut requested_exports = Vec::new();
                let mut loads_namespace = false;
                for specifier in &n.specifiers {
                    match specifier {
                        ExportSpecifier::Named(named) if !named.is_type_only => {
                            requested_exports.push(export_name(&named.orig));
                        }
                        ExportSpecifier::Namespace(_) => loads_namespace = true,
                        ExportSpecifier::Default(_) => {
                            requested_exports.push("default".to_string());
                        }
                        ExportSpecifier::Named(_) => {}
                    }
                }
                if !requested_exports.is_empty() || loads_namespace || n.specifiers.is_empty() {
                    self.record_module_reference(
                        &source,
                        requested_exports,
                        loads_namespace,
                        ModuleReferenceKind::ReExport,
                        n.span(),
                    );
                }
            }
        }
    }

    fn visit_import_decl(&mut self, n: &ImportDecl) {
        let source = n.src.value.to_string_lossy();
        self.record_import(&source, n.src.span);
        if n.type_only {
            return;
        }

        let mut requested_exports = Vec::new();
        let mut loads_namespace = false;
        for specifier in &n.specifiers {
            match specifier {
                ImportSpecifier::Named(named) if !named.is_type_only => {
                    requested_exports.push(
                        named
                            .imported
                            .as_ref()
                            .map(export_name)
                            .unwrap_or_else(|| named.local.sym.to_string()),
                    );
                }
                ImportSpecifier::Default(_) => requested_exports.push("default".to_string()),
                ImportSpecifier::Namespace(_) => loads_namespace = true,
                ImportSpecifier::Named(_) => {}
            }
        }
        if !requested_exports.is_empty() || loads_namespace || n.specifiers.is_empty() {
            self.record_module_reference(
                &source,
                requested_exports,
                loads_namespace,
                ModuleReferenceKind::StaticImport,
                n.span(),
            );
        }
    }

    fn visit_export_all(&mut self, n: &ExportAll) {
        let source = n.src.value.to_string_lossy();
        self.record_import(&source, n.src.span);
        if !n.type_only {
            self.record_module_reference(
                &source,
                Vec::new(),
                true,
                ModuleReferenceKind::ReExport,
                n.span(),
            );
        }
    }

    fn visit_expr(&mut self, n: &Expr) {
        if let Expr::Member(member) = n {
            if let Some((field, span)) = self.static_context_field(member) {
                self.record_context_field(field, span);
            }
        }

        // WebSocket termination is not in the current provider intersection.
        // Observe runtime expressions, including computed property access,
        // without scanning comments, string contents or erased TS types.
        match n {
            Expr::Ident(ident) if ident.ctxt == self.unresolved_context => {
                self.record_provider_only_global(ident.sym.as_ref(), ident.span);
            }
            Expr::Member(member) if self.is_global_object(&member.obj) => match &member.prop {
                MemberProp::Ident(ident) => {
                    self.record_provider_only_global(ident.sym.as_ref(), ident.span);
                }
                MemberProp::Computed(computed) => {
                    if let Expr::Lit(Lit::Str(value)) = &*computed.expr {
                        self.record_provider_only_global(
                            &value.value.to_string_lossy(),
                            value.span,
                        );
                    }
                }
                MemberProp::PrivateName(_) => {}
            },
            _ => {}
        }

        if let Expr::New(expression) = n {
            self.record_non_portable_stream_constructor(expression);
        }

        // These literal forms resolve files just as a static import does.
        // They belong in the staged module graph as well; otherwise a build
        // can ship its eager path and leave its lazy or CommonJS path behind.
        if let Expr::Call(call) = n {
            let dynamic_import = matches!(call.callee, Callee::Import(_));
            let require = matches!(
                &call.callee,
                Callee::Expr(callee)
                    if matches!(&**callee, Expr::Ident(ident) if ident.sym == *"require")
            );
            let imports_module = dynamic_import || require;
            if imports_module {
                if let Some(argument) = call.args.first() {
                    if let Expr::Lit(Lit::Str(specifier)) = &*argument.expr {
                        let literal = specifier.span;
                        let specifier = specifier.value.to_string_lossy();
                        self.record_import(&specifier, literal);
                        self.record_module_reference(
                            &specifier,
                            Vec::new(),
                            true,
                            if dynamic_import {
                                ModuleReferenceKind::DynamicImport
                            } else {
                                ModuleReferenceKind::Require
                            },
                            call.span(),
                        );
                    } else {
                        self.non_literal_module_loads += 1;
                        let location = self.source_map.lookup_char_pos(call.span().lo());
                        self.non_literal_module_load_positions
                            .push(ModuleSourcePosition {
                                line: location.line,
                                column: location.col_display + 1,
                            });
                    }
                } else {
                    self.non_literal_module_loads += 1;
                    let location = self.source_map.lookup_char_pos(call.span().lo());
                    self.non_literal_module_load_positions
                        .push(ModuleSourcePosition {
                            line: location.line,
                            column: location.col_display + 1,
                        });
                }
            }
        }

        // Continue visiting children
        n.visit_children_with(self);
    }

    fn visit_var_declarator(&mut self, n: &VarDeclarator) {
        // Destructuring reads the global property without forming a MemberExpr.
        if let (Pat::Object(pattern), Some(initializer)) = (&n.name, &n.init) {
            if self.is_global_object(initializer) {
                for property in &pattern.props {
                    match property {
                        ObjectPatProp::KeyValue(key_value) => {
                            self.record_provider_only_property_name(&key_value.key);
                        }
                        ObjectPatProp::Assign(assign) => self.record_provider_only_global(
                            assign.key.id.sym.as_ref(),
                            assign.key.id.span,
                        ),
                        ObjectPatProp::Rest(_) => {}
                    }
                }
            }
            if let Expr::Ident(context) = &**initializer {
                if self.context_bindings.contains(&context.to_id()) {
                    for property in &pattern.props {
                        match property {
                            ObjectPatProp::KeyValue(key_value) => match &key_value.key {
                                PropName::Ident(field) => {
                                    self.record_context_field(field.sym.to_string(), field.span)
                                }
                                PropName::Str(field) => self.record_context_field(
                                    field.value.to_string_lossy().into_owned(),
                                    field.span,
                                ),
                                PropName::Computed(computed) => match &*computed.expr {
                                    Expr::Lit(Lit::Str(field)) => self.record_context_field(
                                        field.value.to_string_lossy().into_owned(),
                                        field.span,
                                    ),
                                    _ => self.record_context_field(
                                        "<dynamic>".to_string(),
                                        computed.span,
                                    ),
                                },
                                PropName::Num(number) => {
                                    self.record_context_field(number.value.to_string(), number.span)
                                }
                                PropName::BigInt(number) => {
                                    self.record_context_field(number.value.to_string(), number.span)
                                }
                            },
                            ObjectPatProp::Assign(assign) => self.record_context_field(
                                assign.key.id.sym.to_string(),
                                assign.key.id.span,
                            ),
                            // A rest pattern copies only the four standard
                            // fields that exist at runtime; it does not name a
                            // provider extension and is therefore portable.
                            ObjectPatProp::Rest(_) => {}
                        }
                    }
                }
            }
        }
        n.visit_children_with(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_default_export() {
        let parser = TypeScriptParser::new();
        let code = r#"
export default async function handler(context) {
    return new Response("Hello");
}
"#;

        let result = parser.parse(code, "functions/api/hello.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert!(info.has_default_export);
        assert_eq!(info.route, "/api/hello");
        assert_eq!(info.named_exports.len(), 0);
    }

    /// `export { handler as default }` is a default export, and
    /// `export { default } from "./elsewhere"` is one too. Both used to be
    /// recorded as named exports with no default, which - now that a missing
    /// default export stops a build - would have refused two correct files.
    #[test]
    fn a_default_written_as_a_named_specifier_is_still_a_default() {
        let parser = TypeScriptParser::new();
        for code in [
            "async function handler(c) { return new Response(\"x\"); }\nexport { handler as default };\n",
            "export { default } from \"./elsewhere\";\n",
            "export { default, helper } from \"./elsewhere\";\n",
        ] {
            let info = parser.parse(code, "functions/index.ts").expect("parses");
            assert!(info.has_default_export, "{code}");
            assert!(!info.named_exports.contains(&"default".to_string()), "{code}");
        }
    }

    /// And the mirror image: re-exporting somebody else's default under
    /// another name leaves this file without one.
    #[test]
    fn a_default_re_exported_under_another_name_is_not_this_files_default() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "export { default as report } from \"./elsewhere\";\n",
                "functions/index.ts",
            )
            .expect("parses");
        assert!(!info.has_default_export);
        assert_eq!(info.named_exports, vec!["report".to_string()]);
    }

    #[test]
    fn test_parse_named_exports() {
        let parser = TypeScriptParser::new();
        let code = r#"
export const config = { runtime: 'edge' };
export const handler = async (ctx) => { return new Response("OK"); };
export function middleware() { return null; }
"#;

        let result = parser.parse(code, "functions/api/test.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert!(!info.has_default_export);
        assert!(info.named_exports.contains(&"config".to_string()));
        assert!(info.named_exports.contains(&"handler".to_string()));
        assert!(info.named_exports.contains(&"middleware".to_string()));
    }

    #[test]
    fn test_parse_default_export_arrow_function() {
        let parser = TypeScriptParser::new();
        let code = r#"
export default async (context) => {
    const data = await fetch("https://api.example.com");
    return new Response(data);
}
"#;

        let result = parser.parse(code, "functions/index.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert!(info.has_default_export);
        assert_eq!(info.route, "/");
    }

    // Not covered here: service detection from the AST. Exports, imports and
    // routing are, and those are what the build depends on - which services an
    // application uses is read from `.config.json`, not inferred from its code,
    // so nothing downstream relies on the detection these tests would cover.

    #[test]
    fn test_detect_imports() {
        let parser = TypeScriptParser::new();
        let code = r#"
import { z } from 'zod';
import type { Context } from '@deis/types';
import * as utils from '../utils';

export default async function handler(context: Context) {
    return new Response("OK");
}
"#;

        let result = parser.parse(code, "functions/api/validate.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert_eq!(info.imports.len(), 3);
        assert!(info.imports.contains(&"zod".to_string()));
        assert!(info.imports.contains(&"@deis/types".to_string()));
        assert!(info.imports.contains(&"../utils".to_string()));
    }

    #[test]
    fn every_literal_module_edge_is_recorded_once() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                r#"
import value from './static.js';
export { value as named } from './reexport.js';
export * from './all.js';
const lazy = import('./lazy.js');
const common = require('./common.cjs');
export default async function handler() { return new Response(String(value)); }
"#,
                "functions/index.js",
            )
            .expect("module parses");

        assert_eq!(
            info.imports,
            vec![
                "./static.js",
                "./reexport.js",
                "./all.js",
                "./lazy.js",
                "./common.cjs",
            ]
        );
        assert_eq!(info.non_literal_module_loads, 0);
    }

    #[test]
    fn module_edges_preserve_the_export_surface_and_source_position() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "import crypto, { createHash as hash, type KeyObject } from 'node:crypto';\n\
                 import * as path from 'node:path';\n\
                 import 'node:diagnostics_channel';\n\
                 export { EventEmitter as Events } from 'node:events';\n\
                 export * from 'node:buffer';\n\
                 const timers = require('node:timers');\n\
                 const streams = import('node:stream');\n\
                 export default async function handler() { return new Response(String(hash)); }\n",
                "functions/index.ts",
            )
            .expect("module parses");

        assert_eq!(info.module_references.len(), 7);
        assert_eq!(
            info.module_references[0].requested_exports,
            vec!["default", "createHash"]
        );
        assert!(!info.module_references[0].loads_namespace);
        assert_eq!(info.module_references[0].line, 1);
        assert_eq!(info.module_references[0].column, 1);
        assert!(info.module_references[1].loads_namespace);
        assert!(info.module_references[2].requested_exports.is_empty());
        assert!(!info.module_references[2].loads_namespace);
        assert_eq!(
            info.module_references[3].requested_exports,
            vec!["EventEmitter"]
        );
        assert!(info.module_references[4].loads_namespace);
        assert_eq!(info.module_references[5].kind, ModuleReferenceKind::Require);
        assert_eq!(
            info.module_references[6].kind,
            ModuleReferenceKind::DynamicImport
        );
    }

    #[test]
    fn type_only_edges_do_not_become_runtime_surface_claims() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "import type { KeyObject } from 'node:crypto';\n\
                 export type { EventEmitter } from 'node:events';\n\
                 export default function handler() {}\n",
                "functions/index.ts",
            )
            .expect("module parses");

        assert!(info.module_references.is_empty());
        assert_eq!(info.imports, vec!["node:crypto", "node:events"]);
    }

    #[test]
    fn computed_module_loads_are_counted_for_a_fail_closed_build() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "export default async function handler(name) { await import(name); return require(name); }",
                "functions/index.js",
            )
            .expect("module parses");
        assert_eq!(info.non_literal_module_loads, 2);
        assert_eq!(info.non_literal_module_load_positions.len(), 2);
        assert_eq!(info.non_literal_module_load_positions[0].line, 1);
    }

    #[test]
    fn provider_only_websocket_globals_are_ast_references_not_text_matches() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "type WebSocket = string;\n\
                 const harmless = 'WebSocketPair'; // new WebSocket('ignored')\n\
                 const one = new WebSocket(url);\n\
                 const two = globalThis['WebSocketPair'];\n\
                 const three = self.WebSocketServer;\n\
                 export default function handler() { return new Response(harmless + one + two + three); }\n",
                "functions/index.ts",
            )
            .expect("module parses");
        assert_eq!(info.provider_only_global_references.len(), 3);
        assert_eq!(info.provider_only_global_references[0].name, "WebSocket");
        assert_eq!(info.provider_only_global_references[0].line, 3);
        assert!(info.provider_only_global_references[0].column > 0);
        assert_eq!(
            info.provider_only_global_references[1].name,
            "WebSocketPair"
        );
        assert_eq!(info.provider_only_global_references[1].line, 4);
        assert!(info.provider_only_global_references[1].column > 0);
        assert_eq!(
            info.provider_only_global_references[2].name,
            "WebSocketServer"
        );
        assert_eq!(info.provider_only_global_references[2].line, 5);
    }

    #[test]
    fn local_websocket_names_are_not_provider_globals() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "import { WebSocketPair } from './portable.ts';\n\
                 class WebSocket { constructor(_url: string) {} }\n\
                 export default function handler() {\n\
                   return new Response(String(new WebSocket('local')) + WebSocketPair);\n\
                 }\n",
                "functions/index.ts",
            )
            .expect("module parses");
        assert!(info.provider_only_global_references.is_empty());
    }

    #[test]
    fn global_object_destructuring_is_a_provider_reference() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "const { WebSocket: Socket, ['WebSocketPair']: Pair } = globalThis;\n\
                 export default function handler() { return new Response(String(Socket) + Pair); }\n",
                "functions/index.ts",
            )
            .expect("module parses");
        assert_eq!(info.provider_only_global_references.len(), 2);
        assert_eq!(info.provider_only_global_references[0].name, "WebSocket");
        assert_eq!(
            info.provider_only_global_references[1].name,
            "WebSocketPair"
        );
    }

    #[test]
    fn non_portable_stream_constructors_are_scope_resolved() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "const readable = new ReadableStream();\n\
                 const writable = new self['WritableStream']();\n\
                 const transformed = new globalThis.TransformStream({ transform() {} });\n\
                 const identity = new TransformStream();\n\
                 export default function handler() { return new Response(String(readable) + writable + transformed + identity); }\n",
                "functions/index.ts",
            )
            .expect("module parses");
        let names = info
            .non_portable_stream_constructors
            .iter()
            .map(|reference| reference.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["ReadableStream", "WritableStream", "TransformStream"]
        );
        assert_eq!(info.non_portable_stream_constructors[0].line, 1);
        assert_eq!(info.non_portable_stream_constructors[1].line, 2);
        assert_eq!(info.non_portable_stream_constructors[2].line, 3);
    }

    #[test]
    fn local_stream_constructor_names_are_not_standard_globals() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "class ReadableStream {}\n\
                 class WritableStream {}\n\
                 class TransformStream { constructor(_transformer: unknown) {} }\n\
                 const values = [new ReadableStream(), new WritableStream(), new TransformStream({})];\n\
                 export default function handler() { return new Response(String(values.length)); }\n",
                "functions/index.ts",
            )
            .expect("module parses");
        assert!(info.non_portable_stream_constructors.is_empty());
    }

    #[test]
    fn standard_context_fields_are_exact_and_simple_aliases_are_resolved() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                r#"
                function handler(ctx) {
                  const alias = ctx;
                  const second = alias;
                  const { request, env, params, waitUntil, path } = second;
                  return new Response(String(request) + env + params + waitUntil + path + ctx["services"] + ctx[key]);
                }
                export { handler as default };
                "#,
                "functions/index.ts",
            )
            .expect("module parses");
        let fields = info
            .non_standard_context_references
            .iter()
            .map(|reference| reference.field.as_str())
            .collect::<Vec<_>>();
        assert_eq!(fields, ["path", "services", "<dynamic>"]);
    }

    #[test]
    fn an_event_handler_context_is_its_third_parameter_and_only_platform_fields_are_refused() {
        let parser = TypeScriptParser::new();
        for kind in [HandlerKind::Queue, HandlerKind::Scheduled] {
            let info = parser
                .parse_handler(
                    r#"
                    export default async function handler(context, env, ctx) {
                      const { bindings } = ctx;
                      ctx.waitUntil(ctx.log);
                      return context.services + context.bindings + ctx.services.kv + bindings + env.KV;
                    }
                    "#,
                    "functions/_queue.ts",
                    kind,
                )
                .expect("module parses");
            let fields = info
                .non_standard_context_references
                .iter()
                .map(|reference| reference.field.as_str())
                .collect::<Vec<_>>();
            assert_eq!(fields, ["bindings", "services"], "{kind:?}");
        }
    }

    #[test]
    fn an_unrelated_local_context_object_is_not_the_handler_context() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                r#"
                function helper() {
                  const context = { path: "/local", services: {} };
                  return context.path + String(context.services);
                }
                export default function handler(ctx) {
                  return new Response(ctx.request.url + helper());
                }
                "#,
                "functions/index.ts",
            )
            .expect("module parses");
        assert!(info.non_standard_context_references.is_empty());
    }

    #[test]
    fn direct_default_arrow_context_is_checked() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "export default (requestContext) => new Response(requestContext.method);",
                "functions/index.ts",
            )
            .expect("module parses");
        assert_eq!(info.non_standard_context_references.len(), 1);
        assert_eq!(info.non_standard_context_references[0].field, "method");
    }

    #[test]
    fn default_parameter_destructuring_is_checked() {
        let parser = TypeScriptParser::new();
        for code in [
            "export default ({ request, env, params, waitUntil, path }) => new Response(String(path));",
            "export default function ({ request, ['services']: services } = {}) { return new Response(String(services)); }",
        ] {
            let info = parser
                .parse(code, "functions/index.ts")
                .expect("module parses");
            assert_eq!(info.non_standard_context_references.len(), 1, "{code}");
        }
    }

    #[test]
    fn an_aliased_default_handler_is_checked() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "const implementation = (ctx) => new Response(ctx.path);\n\
                 const published = implementation;\n\
                 export default published;\n",
                "functions/index.ts",
            )
            .expect("module parses");
        assert_eq!(info.non_standard_context_references.len(), 1);
        assert_eq!(info.non_standard_context_references[0].field, "path");
    }

    #[test]
    fn test_path_to_route_index() {
        let route1 = TypeScriptParser::path_to_route("functions/index.ts");
        assert_eq!(route1, "/");

        let route2 = TypeScriptParser::path_to_route("functions/index.js");
        assert_eq!(route2, "/");
    }

    #[test]
    fn test_path_to_route_simple() {
        let route1 = TypeScriptParser::path_to_route("functions/api/users.ts");
        assert_eq!(route1, "/api/users");

        let route2 = TypeScriptParser::path_to_route("functions/hello.js");
        assert_eq!(route2, "/hello");
    }

    #[test]
    fn test_path_to_route_nested() {
        let route1 = TypeScriptParser::path_to_route("functions/api/v1/users.ts");
        assert_eq!(route1, "/api/v1/users");

        let route2 = TypeScriptParser::path_to_route("functions/admin/dashboard/stats.js");
        assert_eq!(route2, "/admin/dashboard/stats");
    }

    #[test]
    fn test_path_to_route_dynamic_params() {
        let route1 = TypeScriptParser::path_to_route("functions/api/users/[id].ts");
        assert_eq!(route1, "/api/users/[id]");

        let route2 = TypeScriptParser::path_to_route("functions/posts/[slug]/comments.js");
        assert_eq!(route2, "/posts/[slug]/comments");
    }

    #[test]
    fn test_parse_typescript_with_types() {
        let parser = TypeScriptParser::new();
        let code = r#"
interface User {
    id: number;
    name: string;
}

export default async function handler(context: Context): Promise<Response> {
    const user: User = { id: 1, name: "Alice" };
    return Response.json(user);
}
"#;

        let result = parser.parse(code, "functions/api/users.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert!(info.has_default_export);
    }

    #[test]
    fn test_parse_mixed_exports() {
        let parser = TypeScriptParser::new();
        let code = r#"
export const config = { runtime: 'edge' };

export default async function handler(context) {
    return new Response("Hello");
}

export const metadata = { version: '1.0.0' };
"#;

        let result = parser.parse(code, "functions/api/hello.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert!(info.has_default_export);
        assert!(info.named_exports.contains(&"config".to_string()));
        assert!(info.named_exports.contains(&"metadata".to_string()));
    }

    #[test]
    fn test_parse_no_exports() {
        let parser = TypeScriptParser::new();
        let code = r#"
function helperFunction() {
    return "helper";
}

const data = { key: "value" };
"#;

        let result = parser.parse(code, "functions/helpers.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert!(!info.has_default_export);
        assert_eq!(info.named_exports.len(), 0);
    }

    #[test]
    fn test_parse_invalid_syntax() {
        let parser = TypeScriptParser::new();
        let code = r#"
export default async function handler(context {
    // Missing closing parenthesis
    return new Response("Error");
}
"#;

        let result = parser.parse(code, "functions/broken.ts");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_jsx_like_syntax() {
        let parser = TypeScriptParser::new();
        let code = r#"
export default async function handler(context) {
    const html = "<div>Hello World</div>";
    return new Response(html, {
        headers: { "content-type": "text/html" }
    });
}
"#;

        let result = parser.parse(code, "functions/page.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert!(info.has_default_export);
    }
}
