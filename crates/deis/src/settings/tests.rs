use super::*;

use std::sync::{Mutex, MutexGuard, OnceLock};

/// The process environment is shared by every test in this binary, and these
/// tests both read and write it. They take this lock so one test's variables
/// are never visible to another.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Clears every variable this module reads, so a test starts from a known
/// state whatever the developer has exported.
fn clear_env() {
    for name in [
        ENV_CONTROL_ENDPOINT,
        ENV_CONTROL_TOKEN,
        ENV_TELEMETRY_ENDPOINT,
        ENV_QUEUE_ENDPOINT,
        ENV_APP,
        ENV_ENVIRONMENT,
        ENV_CONFIG_HOME,
        "XDG_CONFIG_HOME",
    ] {
        // SAFETY: every test that touches the environment holds `env_lock`,
        // so no other thread in this binary is reading it concurrently.
        unsafe { std::env::remove_var(name) };
    }
}

fn set_env(name: &str, value: &str) {
    // SAFETY: as above - the caller holds `env_lock`.
    unsafe { std::env::set_var(name, value) };
}

fn project(dir: &Path, contents: &str) {
    fs::write(dir.join(PROJECT_FILE), contents).unwrap();
}

#[test]
fn the_project_file_supplies_the_addresses_and_the_defaults() {
    let _guard = env_lock();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    project(
        dir.path(),
        r#"
[control]
endpoint = "http://control.example:8085"

[telemetry]
endpoint = "http://telemetry.example:8092"

[queue]
endpoint = "http://queue.example:8091"

[defaults]
app = "shop"
environment = "staging"
"#,
    );

    let settings = Settings::load(dir.path(), &Overrides::default()).unwrap();
    assert_eq!(
        settings.control_endpoint().unwrap(),
        "http://control.example:8085"
    );
    assert_eq!(
        settings.telemetry_endpoint().unwrap(),
        "http://telemetry.example:8092"
    );
    assert_eq!(
        settings.queue_endpoint().unwrap(),
        "http://queue.example:8091"
    );
    assert_eq!(settings.app().unwrap(), "shop");
    assert_eq!(settings.environment(), "staging");
    assert_eq!(settings.app.as_ref().unwrap().source, Source::Project);
}

/// Only the dead letter commands need it, and a checkout that has never
/// named deis-queue must be told where to write the address rather than
/// handed a transport error from a call it could not have made.
#[test]
fn a_project_that_never_named_deis_queue_is_told_both_ways_to_name_it() {
    let _guard = env_lock();
    clear_env();
    let dir = tempfile::tempdir().unwrap();

    let error = Settings::load(dir.path(), &Overrides::default())
        .unwrap()
        .queue_endpoint()
        .unwrap_err()
        .to_string();
    assert!(error.contains("[queue]"), "{error}");
    assert!(error.contains(ENV_QUEUE_ENDPOINT), "{error}");

    set_env(ENV_QUEUE_ENDPOINT, "http://from-env:8091/");
    let settings = Settings::load(dir.path(), &Overrides::default()).unwrap();
    assert_eq!(
        settings.queue_endpoint().unwrap(),
        "http://from-env:8091",
        "a trailing slash is trimmed the way the other two addresses are"
    );
}

#[test]
fn an_environment_variable_beats_the_project_file_and_a_flag_beats_both() {
    let _guard = env_lock();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    project(
        dir.path(),
        "[control]\nendpoint = \"http://from-file:8085\"\n\n[defaults]\napp = \"from-file\"\n",
    );

    set_env(ENV_CONTROL_ENDPOINT, "http://from-env:8085");
    set_env(ENV_APP, "from-env");

    let settings = Settings::load(dir.path(), &Overrides::default()).unwrap();
    assert_eq!(settings.control_endpoint().unwrap(), "http://from-env:8085");
    assert_eq!(settings.app().unwrap(), "from-env");
    assert_eq!(
        settings.app.as_ref().unwrap().source,
        Source::Env(ENV_APP),
        "the source has to name the variable, or `deis config list` cannot say what to edit"
    );

    let overrides = Overrides {
        app: Some("from-flag".to_string()),
        ..Overrides::default()
    };
    let settings = Settings::load(dir.path(), &overrides).unwrap();
    assert_eq!(settings.app().unwrap(), "from-flag");
    assert_eq!(settings.app.as_ref().unwrap().source, Source::Flag);
}

/// With nothing else to go on, the application is the one this project
/// builds - the name that ends up in the bundle manifest.
#[test]
fn the_application_falls_back_to_the_build_configuration() {
    let _guard = env_lock();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".config.json"),
        r#"{"version":"1.0","name":"demo","runtime":"standard-v1","language":"typescript"}"#,
    )
    .unwrap();

    let settings = Settings::load(dir.path(), &Overrides::default()).unwrap();
    assert_eq!(settings.app().unwrap(), "demo");
    assert_eq!(settings.app.as_ref().unwrap().source, Source::BuildConfig);
    assert_eq!(
        settings.environment(),
        deis_wire::conventions::DEFAULT_ENVIRONMENT
    );
}

/// The token lives outside the project, and it is looked up by the address it
/// belongs to so two platforms can be used from one machine.
#[test]
fn the_token_comes_from_the_credentials_file_keyed_by_endpoint() {
    let _guard = env_lock();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    set_env(ENV_CONFIG_HOME, home.path().to_str().unwrap());
    project(
        dir.path(),
        "[control]\nendpoint = \"http://control.example:8085\"\n",
    );

    let mut credentials = Credentials::default();
    credentials.endpoints.insert(
        "http://control.example:8085".to_string(),
        CredentialEntry {
            token: "for-this-one".to_string(),
        },
    );
    credentials.endpoints.insert(
        "http://other.example:8085".to_string(),
        CredentialEntry {
            token: "for-the-other-one".to_string(),
        },
    );
    save_credentials(&credentials).unwrap();

    let settings = Settings::load(dir.path(), &Overrides::default()).unwrap();
    assert_eq!(settings.token(), Some("for-this-one"));
    assert_eq!(settings.token_source(), Some(&Source::Credentials));

    set_env(ENV_CONTROL_TOKEN, "from-the-shell");
    let settings = Settings::load(dir.path(), &Overrides::default()).unwrap();
    assert_eq!(settings.token(), Some("from-the-shell"));
}

#[cfg(unix)]
#[test]
fn the_credentials_file_is_written_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = env_lock();
    clear_env();
    let home = tempfile::tempdir().unwrap();
    set_env(ENV_CONFIG_HOME, home.path().to_str().unwrap());

    let path = save_credentials(&Credentials::default()).unwrap();
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "a credentials file any account on the host can read is a credential that has to be rotated"
    );
}

/// A token in the project file is treated as leaked rather than used: that
/// file is committed, and a committed token is a rotated token.
#[test]
fn a_token_in_the_project_file_is_refused_and_the_message_says_what_to_do() {
    let _guard = env_lock();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    project(
        dir.path(),
        "[control]\nendpoint = \"http://control.example:8085\"\ntoken = \"oops\"\n",
    );

    let error = Settings::load(dir.path(), &Overrides::default())
        .expect_err("a token in a committed file must not be used quietly")
        .to_string();
    assert!(error.contains("rotate"), "{error}");
    assert!(error.contains("deis config set control.token"), "{error}");
}

#[test]
fn a_missing_control_plane_is_an_error_that_names_the_file_and_the_variable() {
    let _guard = env_lock();
    clear_env();
    let dir = tempfile::tempdir().unwrap();

    let settings = Settings::load(dir.path(), &Overrides::default()).unwrap();
    let error = settings.control_endpoint().unwrap_err().to_string();
    assert!(error.contains(PROJECT_FILE), "{error}");
    assert!(error.contains(ENV_CONTROL_ENDPOINT), "{error}");
}

/// The dot is the tenant separator, so a name carrying one produces an
/// application that runs and cannot reach any of its storage.
#[test]
fn a_name_with_a_dot_is_refused_before_anything_is_deployed() {
    let error = check_name("environment", "pr-42.1")
        .unwrap_err()
        .to_string();
    assert!(error.contains("pr-42-1"), "{error}");
    check_name("environment", "pr-42-1").unwrap();
    check_name("application", "demo").unwrap();
}
