//! Deciding, at build time, which handler answers which request.
//!
//! The compiler hands over one `(pattern, import path)` pair per handler file,
//! in whatever order the directory walk produced. Two things have to happen
//! before that list can be baked into an entry module:
//!
//!   1. **Order it by specificity.** The generated matcher takes the first
//!      pattern that fits, so `/users/new` has to be tried before
//!      `/users/:id`, and `/api/*` last of all. Left in walk order, which
//!      handler answers `/users/new` would depend on how the filesystem
//!      happened to list the directory.
//!   2. **Make it deterministic.** That same walk order differs between
//!      machines. The bundle is content-addressed, so a table that reorders
//!      itself changes the version identity of a build whose input never
//!      changed.
//!
//! The rule itself is not written here. deis-runtime applies the same rule to
//! the same patterns at request time, and when the two spell it differently a
//! request reaches a handler the built table does not point at - with no error
//! to go on, because both halves think they are right. So the ordering comes
//! from [`deis_bundle::sort_by_specificity`], the platform's single definition
//! of the rule, which is what deis-runtime ranks the manifest's patterns by
//! too.

use deis_build_core::routes::{
    handler_kind, wildcard_is_reachable, wildcard_name, HandlerKind, QUEUE_HANDLER_STEM,
    SCHEDULED_HANDLER_STEM,
};
use deis_bundle::sort_by_specificity;

/// One handler and the requests it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteEntry {
    /// The pattern as it goes into the manifest: `/users/:id`, `/api/*`.
    pub pattern: String,
    /// Module specifier the entry module imports the handler from.
    pub import_path: String,
    /// Name the catch-all was written with in the file tree, so a handler can
    /// read the captured tail as `context.params.catch` and not only as
    /// `context.params['*']`.
    pub wildcard: Option<String>,
}

/// An ordered route table plus everything about it worth saying out loud.
#[derive(Debug, Clone, Default)]
pub struct RoutePlan {
    pub routes: Vec<RouteEntry>,
    /// Module specifier of the queue consumer, if the project has one. It is
    /// not a route: no request ever reaches it.
    pub queue: Option<String>,
    /// Module specifier of the cron handler, if the project has one.
    pub scheduled: Option<String>,
    /// Problems that do not stop a build but change what the app does. Each
    /// line names the file and the consequence.
    pub warnings: Vec<String>,
}

/// Order the handlers and report what will not work.
///
/// Two files are lifted out of the table first. `_queue` and `_scheduled` are
/// handlers the *platform* calls, not handlers a request reaches, so leaving
/// them in would publish `/_queue` as a URL and put it in the manifest's route
/// list, where the control plane would take it for a real route.
pub fn plan_routes(routes: &[(String, String)]) -> RoutePlan {
    let mut plan = RoutePlan::default();
    let mut entries: Vec<RouteEntry> = Vec::with_capacity(routes.len());

    for (pattern, import_path) in routes {
        match handler_kind(import_path) {
            HandlerKind::Queue => {
                plan.queue = Some(import_path.clone());
                continue;
            }
            HandlerKind::Scheduled => {
                plan.scheduled = Some(import_path.clone());
                continue;
            }
            HandlerKind::Reserved => {
                plan.warnings.push(format!(
                    "{import_path} starts with an underscore, so it is not published as a \
                     route; the platform only calls {QUEUE_HANDLER_STEM} and \
                     {SCHEDULED_HANDLER_STEM}, so this file is never run at all"
                ));
                continue;
            }
            HandlerKind::Http => {}
        }
        entries.push(RouteEntry {
            pattern: pattern.clone(),
            import_path: import_path.clone(),
            wildcard: wildcard_name(import_path),
        });
    }

    sort_by_specificity(&mut entries, |entry| entry.pattern.as_str());

    for entry in entries {
        if let Some(existing) = plan.routes.iter().find(|r| r.pattern == entry.pattern) {
            plan.warnings.push(format!(
                "two handlers claim the route {}: {} answers it, {} is never called",
                entry.pattern, existing.import_path, entry.import_path
            ));
            continue;
        }
        if !wildcard_is_reachable(&entry.pattern) {
            plan.warnings.push(format!(
                "route {} from {} matches nothing: a catch-all stands for the rest of the path, \
                 so it only means something as the last segment",
                entry.pattern, entry.import_path
            ));
        }
        plan.routes.push(entry);
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(pattern: &str, file: &str) -> (String, String) {
        (pattern.to_string(), file.to_string())
    }

    fn patterns(plan: &RoutePlan) -> Vec<&str> {
        plan.routes.iter().map(|r| r.pattern.as_str()).collect()
    }

    #[test]
    fn a_literal_route_is_tried_before_a_placeholder() {
        let plan = plan_routes(&[
            route("/users/:id", "../src/users/[id].ts"),
            route("/users/new", "../src/users/new.ts"),
        ]);
        assert_eq!(patterns(&plan), vec!["/users/new", "/users/:id"]);
    }

    #[test]
    fn a_catch_all_is_tried_last() {
        let plan = plan_routes(&[
            route("/api/*", "../src/api/[[catch]].ts"),
            route("/api/:id", "../src/api/[id].ts"),
            route("/api/health", "../src/api/health.ts"),
        ]);
        assert_eq!(patterns(&plan), vec!["/api/health", "/api/:id", "/api/*"]);
    }

    #[test]
    fn the_leftmost_literal_decides_which_handler_answers() {
        // Both patterns have one placeholder and two segments, and both fit
        // `/admin/settings`. deis-runtime's matcher compares them segment by
        // segment and picks `/admin/:page`; the table baked into the entry
        // module has to reach the same handler, or the admin page is never
        // served and nothing anywhere says why.
        let plan = plan_routes(&[
            route("/:org/settings", "../src/[org]/settings.ts"),
            route("/admin/:page", "../src/admin/[page].ts"),
        ]);
        assert_eq!(patterns(&plan), vec!["/admin/:page", "/:org/settings"]);
    }

    #[test]
    fn a_longer_path_beats_a_shorter_one_at_the_same_shape() {
        let plan = plan_routes(&[
            route("/:id", "../src/[id].ts"),
            route("/users/:id", "../src/users/[id].ts"),
        ]);
        assert_eq!(patterns(&plan), vec!["/users/:id", "/:id"]);
    }

    #[test]
    fn the_order_does_not_depend_on_the_order_the_files_arrived_in() {
        let forward = plan_routes(&[
            route("/", "../src/index.ts"),
            route("/a", "../src/a.ts"),
            route("/b/:id", "../src/b/[id].ts"),
            route("/*", "../src/[[rest]].ts"),
        ]);
        let backward = plan_routes(&[
            route("/*", "../src/[[rest]].ts"),
            route("/b/:id", "../src/b/[id].ts"),
            route("/a", "../src/a.ts"),
            route("/", "../src/index.ts"),
        ]);
        assert_eq!(forward.routes, backward.routes);
    }

    #[test]
    fn the_wildcard_keeps_the_name_it_was_written_with() {
        let plan = plan_routes(&[route("/api/*", "../src/api/[[catch]].ts")]);
        assert_eq!(plan.routes[0].wildcard.as_deref(), Some("catch"));
    }

    #[test]
    fn two_handlers_on_one_route_leaves_one_uncalled_and_says_so() {
        let plan = plan_routes(&[
            route("/users", "../src/users/index.ts"),
            route("/users", "../src/users.ts"),
        ]);
        assert_eq!(plan.routes.len(), 1);
        assert_eq!(plan.warnings.len(), 1);
        assert!(
            plan.warnings[0].contains("never called"),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn the_queue_handler_is_taken_out_of_the_route_table() {
        // Left in, `/_queue` would be a URL anyone could call and the
        // manifest would list it as a route the control plane believes in.
        let plan = plan_routes(&[
            route("/", "../src/index.ts"),
            route("/_queue", "../src/_queue.ts"),
        ]);
        assert_eq!(patterns(&plan), vec!["/"]);
        assert_eq!(plan.queue.as_deref(), Some("../src/_queue.ts"));
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn the_scheduled_handler_is_taken_out_too() {
        let plan = plan_routes(&[route("/_scheduled", "../src/_scheduled.js")]);
        assert!(plan.routes.is_empty());
        assert_eq!(plan.scheduled.as_deref(), Some("../src/_scheduled.js"));
    }

    #[test]
    fn an_underscore_file_the_platform_does_not_know_is_reported_as_dead_code() {
        let plan = plan_routes(&[route("/_helpers", "../src/_helpers.ts")]);
        assert!(plan.routes.is_empty());
        assert!(plan.queue.is_none());
        assert!(
            plan.warnings[0].contains("never run at all"),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn only_the_function_root_names_the_platform_handlers() {
        // A nested file called _queue.ts is one app's helper, not this app's
        // consumer; picking it would silently consume the wrong queue.
        let plan = plan_routes(&[route("/admin/_queue", "../src/admin/_queue.ts")]);
        assert_eq!(patterns(&plan), vec!["/admin/_queue"]);
        assert!(plan.queue.is_none());
    }

    #[test]
    fn a_directory_called_src_does_not_name_the_platform_handlers() {
        // `functions/src/_queue.ts` is a page under a directory an author
        // called `src`. It used to be promoted to the application's queue
        // consumer - the staging prefix `../src/` was stripped as many times
        // as it appeared - so the file stopped answering `/src/_queue` and
        // started being handed every batch off the queue instead.
        let plan = plan_routes(&[
            route("/", "../src/index.ts"),
            route("/src/_queue", "../src/src/_queue.ts"),
            route("/src/_scheduled", "../src/src/_scheduled.ts"),
        ]);
        assert_eq!(patterns(&plan), vec!["/src/_queue", "/src/_scheduled", "/"]);
        assert!(
            plan.queue.is_none(),
            "a nested file must not become the queue consumer"
        );
        assert!(
            plan.scheduled.is_none(),
            "a nested file must not become the cron handler"
        );
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn a_catch_all_in_the_middle_is_reported_as_dead() {
        let plan = plan_routes(&[route("/api/*/edit", "../src/api/[[catch]]/edit.ts")]);
        assert_eq!(
            plan.routes.len(),
            1,
            "still emitted, so the manifest is honest"
        );
        assert!(
            plan.warnings[0].contains("matches nothing"),
            "{:?}",
            plan.warnings
        );
    }
}
