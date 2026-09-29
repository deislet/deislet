use deis_build_core::*;
use swc_common::{sync::Lrc, FileName, SourceMap};
use swc_ecma_ast::*;
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax};
use swc_ecma_visit::{Visit, VisitWith};

pub struct Validator {
    source_map: Lrc<SourceMap>,
}

impl Default for Validator {
    fn default() -> Self {
        Self::new()
    }
}

impl Validator {
    pub fn new() -> Self {
        Self {
            source_map: Lrc::new(SourceMap::default()),
        }
    }

    pub fn validate(
        &self,
        code: &str,
        handler_info: &HandlerInfo,
        handler_kind: deis_build_core::routes::HandlerKind,
        _config: &ProjectConfig,
        _strict: bool,
        result: &mut ValidationResult,
    ) -> anyhow::Result<()> {
        let http = handler_kind == deis_build_core::routes::HandlerKind::Http;
        for reference in handler_info
            .non_standard_context_references
            .iter()
            .filter(|reference| handler_kind.refuses_context_field(&reference.field))
        {
            let suggestion = match reference.field.as_str() {
                "path" | "query" => "Parse context.request.url with new URL(...)".to_string(),
                "method" | "headers" => {
                    format!("Use context.request.{}", reference.field)
                }
                "log" => "Use console.debug/info/warn/error".to_string(),
                "services" | "bindings" => {
                    "Declare a logical binding and use context.env.<NAME>".to_string()
                }
                "cookies" => "Read and parse context.request.headers.get('cookie')".to_string(),
                _ => "Use only context.request, context.env, context.params, and context.waitUntil"
                    .to_string(),
            };
            result.add_error(ValidationError {
                file: handler_info.path.clone(),
                line: reference.line,
                column: reference.column,
                message: if http {
                    format!(
                        "context.{} is outside the selected Edge Canon HTTP context",
                        reference.field
                    )
                } else {
                    format!(
                        "context.{} is not part of any handler's context",
                        reference.field
                    )
                },
                code: "EC_WEB_CONTEXT_FIELD_NONSTANDARD".to_string(),
                severity: Severity::Error,
                suggestion: Some(suggestion),
            });
        }

        // The default export, and only the default export.
        //
        // This used to accept named exports instead, and a file with named
        // exports and no default reported no errors at all - while the build
        // it was meant to speak for produced an application that could not
        // load. Every generated router imports a handler as
        // `import handlerN from "<file>"` and puts `handlerN` in its route
        // table at module scope, so a file with nothing to bind there leaves a
        // free identifier in a module that throws `ReferenceError` while it is
        // being evaluated: the whole application is down, not the one route.
        // `deis_build_core::bundler::BuildBundler` refuses the same thing at
        // the other end, on the bundler's own report; this is the half that
        // can name the author's own file.
        if !handler_info.has_default_export {
            let named = if handler_info.named_exports.is_empty() {
                String::new()
            } else {
                format!(
                    " It exports {} by name ({}), and named exports are not what a router \
                     imports.",
                    handler_info.named_exports.len(),
                    handler_info.named_exports.join(", ")
                )
            };
            result.add_error(ValidationError {
                file: handler_info.path.clone(),
                line: 1,
                column: 1,
                message: format!(
                    "Handler has no default export, so the generated router has nothing to call \
                     and the whole application fails to load - every route, not only this one.\
                     {named}"
                ),
                code: "NO_EXPORTS".to_string(),
                severity: Severity::Error,
                suggestion: Some(
                    "Add: export default async function handler(context: Context): Promise<Response>"
                        .to_string(),
                ),
            });
        }

        // Parse for deeper validation
        let source_file = self.source_map.new_source_file(
            FileName::Custom(handler_info.path.clone()).into(),
            code.to_string(),
        );

        let lexer = Lexer::new(
            Syntax::Typescript(Default::default()),
            EsVersion::Es2022,
            StringInput::from(&*source_file),
            None,
        );

        let mut parser = Parser::new_from(lexer);
        if let Ok(module) = parser.parse_module() {
            let mut visitor = ValidationVisitor::new(&handler_info.path, result);
            module.visit_with(&mut visitor);
        }

        Ok(())
    }
}

struct ValidationVisitor<'a> {
    file: &'a str,
    result: &'a mut ValidationResult,
}

impl<'a> ValidationVisitor<'a> {
    fn new(file: &'a str, result: &'a mut ValidationResult) -> Self {
        Self { file, result }
    }

    fn add_error(
        &mut self,
        line: usize,
        message: String,
        code: String,
        suggestion: Option<String>,
    ) {
        self.result.add_error(ValidationError {
            file: self.file.to_string(),
            line,
            column: 1,
            message,
            code,
            severity: Severity::Error,
            suggestion,
        });
    }
}

impl<'a> Visit for ValidationVisitor<'a> {
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        // Check for typeof checks on platform globals
        let expr_str = format!("{n:?}");

        if expr_str.contains("typeof Deno")
            || expr_str.contains("typeof Cloudflare")
            || expr_str.contains("typeof EdgeRuntime")
        {
            self.add_error(
                1, // Line number would need span information
                "Platform-specific code detected: You cannot check for platform type".to_string(),
                "PLATFORM_DETECTION".to_string(),
                Some("Use the selected Edge Canon context and Web APIs".to_string()),
            );
        }
    }

    fn visit_member_expr(&mut self, n: &MemberExpr) {
        // Check for context.raw access
        if let Expr::Member(inner) = &*n.obj {
            if let (Expr::Ident(obj), MemberProp::Ident(prop)) = (&*inner.obj, &inner.prop) {
                if obj.sym.as_ref() == "context" && prop.sym.as_ref() == "raw" {
                    self.add_error(
                        1,
                        "Direct access to context.raw is prohibited".to_string(),
                        "RAW_ACCESS".to_string(),
                        Some("Use unified context interfaces instead".to_string()),
                    );
                }
            }
        }

        // Check for platform-specific APIs
        if let Expr::Ident(obj) = &*n.obj {
            if obj.sym.as_ref() == "Deno" || obj.sym.as_ref() == "process" {
                if let MemberProp::Ident(prop) = &n.prop {
                    if prop.sym.as_ref() == "env" {
                        self.add_error(
                            1,
                            format!("Platform-specific API detected: {}.env", obj.sym),
                            "PLATFORM_API".to_string(),
                            Some("Use context.env instead".to_string()),
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_mock_handler_info(path: &str, has_default: bool) -> HandlerInfo {
        HandlerInfo {
            path: path.to_string(),
            route: "/test".to_string(),
            has_default_export: has_default,
            named_exports: vec![],
            used_services: vec![],
            imports: vec![],
            specifier_literals: vec![],
            module_references: vec![],
            non_literal_module_loads: 0,
            non_literal_module_load_positions: vec![],
            provider_only_global_references: vec![],
            non_portable_stream_constructors: vec![],
            non_standard_context_references: vec![],
        }
    }

    /// Built by deserialising rather than by naming every field: a config is
    /// only ever produced from `.config.json`, and an exhaustive literal here
    /// would break every time the format grows an optional key.
    fn create_mock_config() -> ProjectConfig {
        serde_json::from_value(serde_json::json!({
            "version": "0.1.0",
            "name": "test",
            "runtime": "edge",
            "language": "typescript",
            "functionRoot": "functions"
        }))
        .expect("the mock project config parses")
    }

    #[test]
    fn test_valid_handler_with_default_export() {
        let validator = Validator::new();
        let code = r#"
export default async function handler(context) {
    return new Response("OK");
}
"#;
        let handler_info = create_mock_handler_info("test.ts", true);
        let mut result = ValidationResult::new();

        validator
            .validate(
                code,
                &handler_info,
                deis_build_core::routes::HandlerKind::Http,
                &create_mock_config(),
                false,
                &mut result,
            )
            .unwrap();

        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_missing_exports() {
        let validator = Validator::new();
        let code = r#"
function handler(context) {
    return new Response("OK");
}
"#;
        let handler_info = create_mock_handler_info("test.ts", false);
        let mut result = ValidationResult::new();

        validator
            .validate(
                code,
                &handler_info,
                deis_build_core::routes::HandlerKind::Http,
                &create_mock_config(),
                false,
                &mut result,
            )
            .unwrap();

        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].code, "NO_EXPORTS");
        assert_eq!(result.errors[0].severity, Severity::Error);
    }

    // Not covered here: the style rules that walk the AST looking for
    // discouraged patterns. What is covered is the check that decides whether
    // a build succeeds - an entry module with no usable export - so a gap in
    // the style rules costs advice, never a wrong build result.

    #[test]
    fn test_valid_context_usage() {
        let validator = Validator::new();
        let code = r#"
export default async function handler(context) {
    const request = context.request;
    const env = context.env;
    const params = context.params;
    context.waitUntil(Promise.resolve());

    return new Response(request.url + env + params);
}
"#;
        let handler_info = create_mock_handler_info("test.ts", true);
        let mut result = ValidationResult::new();

        validator
            .validate(
                code,
                &handler_info,
                deis_build_core::routes::HandlerKind::Http,
                &create_mock_config(),
                false,
                &mut result,
            )
            .unwrap();

        // Should not have any errors for proper context usage
        assert!(result.errors.is_empty());
    }

    /// Named exports without a default used to pass, and the build that
    /// followed produced a worker that threw `ReferenceError` on load - every
    /// route down, with a green build behind it.
    #[test]
    fn named_exports_without_a_default_are_not_enough() {
        let validator = Validator::new();
        let code = r#"
export const config = { runtime: 'edge' };
export function middleware(req) { return req; }
"#;
        let mut handler_info = create_mock_handler_info("test.ts", false);
        handler_info.named_exports = vec!["config".to_string(), "middleware".to_string()];
        let mut result = ValidationResult::new();

        validator
            .validate(
                code,
                &handler_info,
                deis_build_core::routes::HandlerKind::Http,
                &create_mock_config(),
                false,
                &mut result,
            )
            .unwrap();

        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].code, "NO_EXPORTS");
        // The message has to name what the file does have, or an author
        // reading "no export" while looking at two `export` keywords stops
        // believing the tool.
        assert!(
            result.errors[0].message.contains("middleware"),
            "{:?}",
            result.errors[0]
        );
        assert!(
            result.errors[0].message.contains("whole application"),
            "the cost is the whole app, not the route: {:?}",
            result.errors[0]
        );
    }

    #[test]
    fn test_validation_result_severity() {
        let validator = Validator::new();
        let code = r#"
function notExported() {
    return new Response("Not exported");
}
"#;
        let handler_info = create_mock_handler_info("test.ts", false);
        let mut result = ValidationResult::new();

        validator
            .validate(
                code,
                &handler_info,
                deis_build_core::routes::HandlerKind::Http,
                &create_mock_config(),
                false,
                &mut result,
            )
            .unwrap();

        assert!(!result.errors.is_empty());
        assert_eq!(result.errors[0].severity, Severity::Error);
    }

    #[test]
    fn event_contexts_are_not_misclassified_as_http_contexts() {
        let validator = Validator::new();
        let code = "export default function handler(context) { return context.scheduledTime; }";
        let mut handler_info = create_mock_handler_info("_scheduled.ts", true);
        handler_info
            .non_standard_context_references
            .push(NonStandardContextReference {
                field: "scheduledTime".to_string(),
                line: 1,
                column: 60,
            });
        let mut result = ValidationResult::new();

        validator
            .validate(
                code,
                &handler_info,
                deis_build_core::routes::HandlerKind::Scheduled,
                &create_mock_config(),
                false,
                &mut result,
            )
            .unwrap();

        assert!(result.errors.is_empty());
    }

    #[test]
    fn event_contexts_refuse_the_platform_fields_an_http_context_refuses() {
        let validator = Validator::new();
        for kind in [
            deis_build_core::routes::HandlerKind::Queue,
            deis_build_core::routes::HandlerKind::Scheduled,
        ] {
            for field in ["bindings", "services"] {
                let code = format!(
                    "export default function handler(batch, env, context) {{ return context.{field}; }}"
                );
                let mut handler_info = create_mock_handler_info("_queue.ts", true);
                handler_info
                    .non_standard_context_references
                    .push(NonStandardContextReference {
                        field: field.to_string(),
                        line: 1,
                        column: 70,
                    });
                let mut result = ValidationResult::new();

                validator
                    .validate(
                        &code,
                        &handler_info,
                        kind,
                        &create_mock_config(),
                        false,
                        &mut result,
                    )
                    .unwrap();

                assert_eq!(result.errors.len(), 1, "{kind:?} {field}");
                assert_eq!(result.errors[0].code, "EC_WEB_CONTEXT_FIELD_NONSTANDARD");
            }
        }
    }
}
