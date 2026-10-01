//! Generic OS effects. Lua chooses what to open or copy; this boundary reports outcomes.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use mlua::{Lua, Table, Value};

#[derive(Debug, thiserror::Error)]
pub(crate) enum OperationError {
    #[error("{0}")]
    Invalid(String),
    #[error("failed to launch {program}: {source}")]
    Launch {
        program: &'static str,
        source: std::io::Error,
    },
    #[error("{program} exited with {status}")]
    Status {
        program: &'static str,
        status: std::process::ExitStatus,
    },
    #[error("clipboard initialization failed: {0}")]
    ClipboardInit(arboard::Error),
    #[error("clipboard write failed: {0}")]
    ClipboardWrite(arboard::Error),
    #[error("external opening is unsupported on this platform")]
    Unsupported,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExternalTarget {
    Uri(String),
    Path(PathBuf),
}

fn string(value: Value, name: &str) -> Result<String, OperationError> {
    match value {
        Value::String(value) => value
            .to_str()
            .map(|value| value.to_string())
            .map_err(|_| OperationError::Invalid(format!("{name} must be UTF-8"))),
        _ => Err(OperationError::Invalid(format!("{name} must be a string"))),
    }
}

fn target_string(value: Value, name: &str) -> Result<String, OperationError> {
    let value = string(value, name)?;
    if value.is_empty() || value.contains('\0') {
        return Err(OperationError::Invalid(format!(
            "{name} must be non-empty and contain no NUL bytes"
        )));
    }
    Ok(value)
}

fn field(table: &Table, name: &str) -> Result<Value, OperationError> {
    table
        .raw_get(name)
        .map_err(|error| OperationError::Invalid(error.to_string()))
}

fn parse_target(value: Value) -> Result<ExternalTarget, OperationError> {
    let Value::Table(spec) = value else {
        return Err(OperationError::Invalid(
            "open_external expects a table with uri or path".into(),
        ));
    };
    let uri = field(&spec, "uri")?;
    let path = field(&spec, "path")?;
    let base = field(&spec, "base")?;
    match (uri, path) {
        (Value::Nil, Value::Nil) => Err(OperationError::Invalid(
            "open_external requires uri or path".into(),
        )),
        (uri, Value::Nil) => {
            if !matches!(base, Value::Nil) {
                return Err(OperationError::Invalid("base applies only to path".into()));
            }
            Ok(ExternalTarget::Uri(target_string(uri, "uri")?))
        }
        (Value::Nil, path) => {
            let path = PathBuf::from(target_string(path, "path")?);
            let base = match base {
                Value::Nil => None,
                value => {
                    let base = PathBuf::from(target_string(value, "base")?);
                    if !base.is_absolute() {
                        return Err(OperationError::Invalid("base must be absolute".into()));
                    }
                    Some(base)
                }
            };
            if path.is_absolute() {
                Ok(ExternalTarget::Path(path))
            } else {
                let base = base.ok_or_else(|| {
                    OperationError::Invalid(
                        "relative path requires an explicit absolute base".into(),
                    )
                })?;
                Ok(ExternalTarget::Path(base.join(path)))
            }
        }
        _ => Err(OperationError::Invalid(
            "open_external accepts exactly one of uri or path".into(),
        )),
    }
}

#[derive(Clone, Copy)]
enum Platform {
    Mac,
    Windows,
    Unix,
    Unsupported,
}

struct OpenCommand {
    program: &'static str,
    args: Vec<OsString>,
}

fn open_command(
    target: &ExternalTarget,
    platform: Platform,
) -> Result<OpenCommand, OperationError> {
    let argument = match target {
        ExternalTarget::Uri(uri) => OsString::from(uri),
        ExternalTarget::Path(path) => path.as_os_str().to_owned(),
    };
    let (program, args) = match platform {
        Platform::Mac => ("open", vec![OsString::from("--"), argument]),
        Platform::Windows => ("explorer.exe", vec![argument]),
        Platform::Unix => ("xdg-open", vec![argument]),
        Platform::Unsupported => return Err(OperationError::Unsupported),
    };
    Ok(OpenCommand { program, args })
}

fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Mac
    } else if cfg!(windows) {
        Platform::Windows
    } else if cfg!(unix) {
        Platform::Unix
    } else {
        Platform::Unsupported
    }
}

trait OsEffects: Send + Sync + 'static {
    fn open(&self, target: ExternalTarget) -> Result<(), OperationError>;
    fn copy(&self, text: String) -> Result<(), OperationError>;
}

struct SystemEffects;

impl OsEffects for SystemEffects {
    fn open(&self, target: ExternalTarget) -> Result<(), OperationError> {
        let command = open_command(&target, current_platform())?;
        // Never inherit plugin stdout: it carries the NCP protocol, not OS utility output.
        let status = Command::new(command.program)
            .args(command.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|source| OperationError::Launch {
                program: command.program,
                source,
            })?;
        check_status(command.program, status)
    }

    fn copy(&self, text: String) -> Result<(), OperationError> {
        let mut clipboard = arboard::Clipboard::new().map_err(OperationError::ClipboardInit)?;
        clipboard
            .set_text(text)
            .map_err(OperationError::ClipboardWrite)
    }
}

fn check_status(
    program: &'static str,
    status: std::process::ExitStatus,
) -> Result<(), OperationError> {
    if status.success() {
        Ok(())
    } else {
        Err(OperationError::Status { program, status })
    }
}

fn outcome(result: Result<(), OperationError>) -> (Option<bool>, Option<String>) {
    match result {
        Ok(()) => (Some(true), None),
        Err(error) => (None, Some(error.to_string())),
    }
}

pub(crate) fn install_os_operations(lua: &Lua, tui: &Table) -> mlua::Result<()> {
    install_with_effects(lua, tui, Arc::new(SystemEffects))
}

fn install_with_effects(lua: &Lua, tui: &Table, effects: Arc<dyn OsEffects>) -> mlua::Result<()> {
    let open_effects = Arc::clone(&effects);
    tui.set(
        "open_external",
        lua.create_function(move |_, spec: Value| {
            Ok(outcome(
                parse_target(spec).and_then(|target| open_effects.open(target)),
            ))
        })?,
    )?;
    tui.set(
        "copy_to_clipboard",
        lua.create_function(move |_, text: Value| {
            Ok(outcome(
                string(text, "text").and_then(|text| effects.copy(text)),
            ))
        })?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeEffects {
        opened: Mutex<Vec<ExternalTarget>>,
        copied: Mutex<Vec<String>>,
        fail: bool,
    }

    impl OsEffects for FakeEffects {
        fn open(&self, target: ExternalTarget) -> Result<(), OperationError> {
            self.opened.lock().unwrap().push(target);
            if self.fail {
                Err(OperationError::Invalid("fake open failure".into()))
            } else {
                Ok(())
            }
        }
        fn copy(&self, text: String) -> Result<(), OperationError> {
            self.copied.lock().unwrap().push(text);
            if self.fail {
                Err(OperationError::Invalid("fake clipboard failure".into()))
            } else {
                Ok(())
            }
        }
    }

    fn lua(effects: Arc<FakeEffects>) -> Lua {
        let lua = Lua::new();
        let tui = lua.create_table().unwrap();
        install_with_effects(&lua, &tui, effects).unwrap();
        lua.globals().set("tui", tui).unwrap();
        lua
    }

    #[test]
    fn explicit_targets_preserve_uri_and_anchor_paths() {
        let effects = Arc::new(FakeEffects::default());
        let lua = lua(Arc::clone(&effects));
        let base = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        lua.globals().set("base", base.clone()).unwrap();
        lua.load(
            r#"
            assert(tui.open_external { uri = 'custom:opaque?x=a b#fragment' })
            assert(tui.open_external { path = 'https:filename', base = base })
            assert(tui.open_external { path = base })
            assert(tui.copy_to_clipboard('hello\nworld'))
            assert(tui.copy_to_clipboard(''))
        "#,
        )
        .exec()
        .unwrap();
        assert_eq!(
            *effects.opened.lock().unwrap(),
            vec![
                ExternalTarget::Uri("custom:opaque?x=a b#fragment".into()),
                ExternalTarget::Path(PathBuf::from(&base).join("https:filename")),
                ExternalTarget::Path(PathBuf::from(base)),
            ]
        );
        assert_eq!(*effects.copied.lock().unwrap(), vec!["hello\nworld", ""]);
    }

    #[test]
    fn validation_errors_are_values_and_do_not_execute_effects() {
        let effects = Arc::new(FakeEffects::default());
        let lua = lua(Arc::clone(&effects));
        lua.load(
            r#"
            for _, spec in ipairs({false, 'uri', {}, {uri=3}, {path=3},
                {uri='x',path='y'}, {uri='x',base='/tmp'}, {path='relative'},
                {path='relative',base='relative'}, {path='',base='/tmp'},
                {uri=''}, {uri='x\0y'}}) do
                local ok, err = tui.open_external(spec)
                assert(ok == nil and type(err) == 'string')
            end
            local ok, err = tui.copy_to_clipboard(false)
            assert(ok == nil and type(err) == 'string')
        "#,
        )
        .exec()
        .unwrap();
        assert!(effects.opened.lock().unwrap().is_empty());
        assert!(effects.copied.lock().unwrap().is_empty());
    }

    #[test]
    fn effect_failures_are_observable_without_lua_exceptions() {
        let lua = lua(Arc::new(FakeEffects {
            fail: true,
            ..FakeEffects::default()
        }));
        lua.load(
            r#"
            local ok, err = tui.open_external {uri='mailto:user@example.test'}
            assert(ok == nil and err == 'fake open failure')
            ok, err = tui.copy_to_clipboard('text')
            assert(ok == nil and err == 'fake clipboard failure')
            tui.copy_to_clipboard('callers can ignore the outcome')
        "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn platform_commands_keep_targets_as_one_literal_argument() {
        let target = ExternalTarget::Uri("custom:a b;$(not-a-shell)".into());
        for (platform, program, prefix) in [
            (Platform::Mac, "open", vec![OsString::from("--")]),
            (Platform::Unix, "xdg-open", vec![]),
            (Platform::Windows, "explorer.exe", vec![]),
        ] {
            let command = open_command(&target, platform).unwrap();
            assert_eq!(command.program, program);
            assert_eq!(
                command.args,
                [prefix, vec![OsString::from("custom:a b;$(not-a-shell)")]].concat()
            );
        }
        assert!(matches!(
            open_command(&target, Platform::Unsupported),
            Err(OperationError::Unsupported)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_status_is_not_success() {
        use std::os::unix::process::ExitStatusExt;
        assert!(check_status("fake", std::process::ExitStatus::from_raw(0)).is_ok());
        assert!(matches!(
            check_status("fake", std::process::ExitStatus::from_raw(256)),
            Err(OperationError::Status { .. })
        ));
    }
}
