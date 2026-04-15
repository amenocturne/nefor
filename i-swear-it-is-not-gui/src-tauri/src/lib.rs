use font_kit::source::SystemSource;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::ipc::Channel;
use tauri::Manager;

// --- Preferences ---

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenedProject {
    path: String,
    last_opened: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Keybindings {
    #[serde(default = "default_kb_settings")]
    settings: String,
    #[serde(default = "default_kb_new_agent")]
    new_agent: String,
    #[serde(default = "default_kb_spotlight")]
    spotlight: String,
    #[serde(default = "default_kb_kill_agent")]
    kill_agent: String,
    #[serde(default = "default_kb_next_agent")]
    next_agent: String,
    #[serde(default = "default_kb_prev_agent")]
    prev_agent: String,
    #[serde(default = "default_kb_zoom_in")]
    zoom_in: String,
    #[serde(default = "default_kb_zoom_out")]
    zoom_out: String,
    #[serde(default = "default_kb_zoom_reset")]
    zoom_reset: String,
    #[serde(default = "default_kb_refresh_terminal")]
    refresh_terminal: String,
}

fn default_kb_settings() -> String { "meta+,".to_string() }
fn default_kb_new_agent() -> String { "meta+k".to_string() }
fn default_kb_spotlight() -> String { "meta+o".to_string() }
fn default_kb_kill_agent() -> String { "meta+w".to_string() }
fn default_kb_next_agent() -> String { "meta+]".to_string() }
fn default_kb_prev_agent() -> String { "meta+[".to_string() }
fn default_kb_zoom_in() -> String { "meta+=".to_string() }
fn default_kb_zoom_out() -> String { "meta+-".to_string() }
fn default_kb_zoom_reset() -> String { "meta+0".to_string() }
fn default_kb_refresh_terminal() -> String { "meta+shift+r".to_string() }

impl Default for Keybindings {
    fn default() -> Self {
        Keybindings {
            settings: default_kb_settings(),
            new_agent: default_kb_new_agent(),
            spotlight: default_kb_spotlight(),
            kill_agent: default_kb_kill_agent(),
            next_agent: default_kb_next_agent(),
            prev_agent: default_kb_prev_agent(),
            zoom_in: default_kb_zoom_in(),
            zoom_out: default_kb_zoom_out(),
            zoom_reset: default_kb_zoom_reset(),
            refresh_terminal: default_kb_refresh_terminal(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Preferences {
    font: String,
    #[serde(default = "default_font_size")]
    font_size: u16,
    #[serde(default = "default_theme")]
    theme: String,
    ask_to_install_pi: bool,
    #[serde(default = "default_true")]
    confirm_kill_agent: bool,
    opened_projects: Vec<OpenedProject>,
    #[serde(default)]
    keybindings: Keybindings,
}

fn default_true() -> bool {
    true
}

fn default_font_size() -> u16 {
    14
}

fn default_theme() -> String {
    "dark".to_string()
}

impl Default for Preferences {
    fn default() -> Self {
        Preferences {
            font: "JetBrainsMono NFM".to_string(),
            font_size: 14,
            theme: "dark".to_string(),
            ask_to_install_pi: true,
            confirm_kill_agent: true,
            opened_projects: vec![],
            keybindings: Keybindings::default(),
        }
    }
}

fn preferences_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::Path::new(&home).join(".nefor").join("preferences.json")
}

#[tauri::command]
fn get_preferences() -> Result<Preferences, String> {
    let path = preferences_path();
    if !path.exists() {
        return Ok(Preferences::default());
    }
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read preferences: {e}"))?;
    let prefs: Preferences = serde_json::from_str(&contents)
        .map_err(|e| format!("Failed to parse preferences: {e}"))?;
    Ok(prefs)
}

#[tauri::command]
fn save_preferences(prefs: Preferences) -> Result<(), String> {
    let path = preferences_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create ~/.nefor/: {e}"))?;
    }
    let json = serde_json::to_string_pretty(&prefs)
        .map_err(|e| format!("Failed to serialize preferences: {e}"))?;
    std::fs::write(&path, json)
        .map_err(|e| format!("Failed to write preferences: {e}"))?;
    Ok(())
}

#[tauri::command]
fn check_pi_config(cwd: String) -> Result<bool, String> {
    let pi_dir = std::path::Path::new(&cwd).join(".pi");
    Ok(pi_dir.is_dir())
}

#[tauri::command]
fn install_pi_config(cwd: String) -> Result<(), String> {
    let pi_dir = std::path::Path::new(&cwd).join(".pi");
    std::fs::create_dir_all(&pi_dir)
        .map_err(|e| format!("Failed to create .pi/ directory: {e}"))?;

    let settings = serde_json::json!({
        "provider": "openrouter",
        "models": {
            "orchestrator": "anthropic/claude-sonnet-4",
            "worker": "anthropic/claude-sonnet-4",
            "reviewer": "anthropic/claude-sonnet-4",
            "explorer": "anthropic/claude-sonnet-4",
            "tester": "anthropic/claude-sonnet-4",
            "promptEngineer": "anthropic/claude-sonnet-4"
        }
    });

    let settings_path = pi_dir.join("settings.json");
    let json = serde_json::to_string_pretty(&settings)
        .map_err(|e| format!("Failed to serialize settings: {e}"))?;
    std::fs::write(&settings_path, json)
        .map_err(|e| format!("Failed to write settings.json: {e}"))?;

    Ok(())
}

#[tauri::command]
fn get_project_config(project_path: String) -> Result<serde_json::Value, String> {
    let settings_path = std::path::Path::new(&project_path).join(".pi").join("settings.json");
    if !settings_path.exists() {
        return Err("No .pi/settings.json found".to_string());
    }
    let contents = std::fs::read_to_string(&settings_path)
        .map_err(|e| format!("Failed to read settings: {e}"))?;
    let config: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|e| format!("Failed to parse settings: {e}"))?;
    Ok(config)
}

#[tauri::command]
fn save_project_config(project_path: String, config: serde_json::Value) -> Result<(), String> {
    let pi_dir = std::path::Path::new(&project_path).join(".pi");
    std::fs::create_dir_all(&pi_dir)
        .map_err(|e| format!("Failed to create .pi/ directory: {e}"))?;
    let settings_path = pi_dir.join("settings.json");
    let json = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("Failed to serialize config: {e}"))?;
    std::fs::write(&settings_path, json)
        .map_err(|e| format!("Failed to write settings: {e}"))?;
    Ok(())
}

static AGENT_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Serialize)]
#[serde(tag = "type", content = "payload")]
enum PtyEvent {
    Output(String),
    Exit(i32),
}

#[derive(Clone, Serialize)]
struct AgentInfo {
    id: String,
    name: String,
    cwd: String,
    status: String,
}

struct Agent {
    name: String,
    cwd: String,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
}

struct AppState {
    agents: Arc<Mutex<HashMap<String, Agent>>>,
}

fn generate_agent_name(cwd: &str, existing_agents: &HashMap<String, Agent>) -> String {
    let basename = std::path::Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.to_string());

    let existing_count = existing_agents
        .values()
        .filter(|a| {
            let a_base = a.name.split('-').next().unwrap_or(&a.name);
            // Check if name matches basename exactly or basename-N pattern
            a.name == basename || (a.name.starts_with(&basename) && a_base == basename)
        })
        .count();

    if existing_count == 0 {
        basename
    } else {
        format!("{}-{}", basename, existing_count + 1)
    }
}

/// Build the command to run pi inside a login shell.
/// GUI apps on macOS don't inherit shell PATH, so we always go through
/// a login shell to pick up the user's environment (homebrew, nvm, etc.).
/// This matches how pi behaves when launched from a terminal.
fn build_pi_command() -> (String, Vec<String>) {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());

    // Run: $SHELL -l -c 'pi'
    // The login shell loads .zprofile/.bash_profile which sets up PATH,
    // then execs pi. If pi isn't found, the shell shows the error naturally.
    (shell, vec!["-l".to_string(), "-c".to_string(), "pi".to_string()])
}

#[tauri::command]
fn spawn_agent(
    cwd: String,
    cols: Option<u16>,
    rows: Option<u16>,
    on_event: Channel<PtyEvent>,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let pty_system = native_pty_system();

    let pair = pty_system
        .openpty(PtySize {
            rows: rows.unwrap_or(24),
            cols: cols.unwrap_or(80),
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("Failed to open PTY: {e}"))?;

    // Launch pi through a login shell for consistent env (PATH, etc.)
    let (program, args) = build_pi_command();

    let mut cmd = CommandBuilder::new(&program);
    for arg in &args {
        cmd.arg(arg);
    }
    cmd.cwd(&cwd);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("Failed to spawn shell: {e}"))?;

    drop(pair.slave);

    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("Failed to get PTY writer: {e}"))?;

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Failed to get PTY reader: {e}"))?;

    let id = format!("agent-{}", AGENT_COUNTER.fetch_add(1, Ordering::Relaxed));

    let mut agents = state
        .agents
        .lock()
        .map_err(|e| format!("Lock poisoned: {e}"))?;

    let name = generate_agent_name(&cwd, &agents);

    let agent = Agent {
        name,
        cwd: cwd.clone(),
        writer: Arc::new(Mutex::new(writer)),
        master: Arc::new(Mutex::new(pair.master)),
        child: Arc::new(Mutex::new(child)),
    };

    agents.insert(id.clone(), agent);
    drop(agents);

    let channel = on_event;
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    let _ = channel.send(PtyEvent::Output(text));
                }
                Err(_) => break,
            }
        }
        let _ = channel.send(PtyEvent::Exit(0));
    });

    Ok(id)
}

#[tauri::command]
fn list_agents(state: tauri::State<'_, AppState>) -> Result<Vec<AgentInfo>, String> {
    let agents = state
        .agents
        .lock()
        .map_err(|e| format!("Lock poisoned: {e}"))?;

    let infos: Vec<AgentInfo> = agents
        .iter()
        .map(|(id, agent)| {
            let status = match agent.child.lock() {
                Ok(mut child) => match child.try_wait() {
                    Ok(Some(_)) => "exited".to_string(),
                    Ok(None) => "running".to_string(),
                    Err(_) => "unknown".to_string(),
                },
                Err(_) => "unknown".to_string(),
            };

            AgentInfo {
                id: id.clone(),
                name: agent.name.clone(),
                cwd: agent.cwd.clone(),
                status,
            }
        })
        .collect();

    Ok(infos)
}

#[tauri::command]
fn write_input(agent_id: String, data: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let agents = state
        .agents
        .lock()
        .map_err(|e| format!("Lock poisoned: {e}"))?;

    let agent = agents
        .get(&agent_id)
        .ok_or_else(|| format!("Agent not found: {agent_id}"))?;

    agent
        .writer
        .lock()
        .map_err(|e| format!("Writer lock poisoned: {e}"))?
        .write_all(data.as_bytes())
        .map_err(|e| format!("Write failed: {e}"))?;

    Ok(())
}

#[tauri::command]
fn resize_pty(
    agent_id: String,
    cols: u16,
    rows: u16,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let agents = state
        .agents
        .lock()
        .map_err(|e| format!("Lock poisoned: {e}"))?;

    let agent = agents
        .get(&agent_id)
        .ok_or_else(|| format!("Agent not found: {agent_id}"))?;

    agent
        .master
        .lock()
        .map_err(|e| format!("Master lock poisoned: {e}"))?
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("Resize failed: {e}"))?;

    Ok(())
}

#[tauri::command]
fn kill_agent(agent_id: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut agents = state
        .agents
        .lock()
        .map_err(|e| format!("Lock poisoned: {e}"))?;

    let agent = agents
        .remove(&agent_id)
        .ok_or_else(|| format!("Agent not found: {agent_id}"))?;

    agent
        .child
        .lock()
        .map_err(|e| format!("Child lock poisoned: {e}"))?
        .kill()
        .map_err(|e| format!("Kill failed: {e}"))?;

    Ok(())
}

#[tauri::command]
fn get_pi_binary_path() -> Result<String, String> {
    // Check PATH first
    if let Ok(output) = std::process::Command::new("which").arg("pi").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(path);
            }
        }
    }

    // Common install locations
    for candidate in &[
        "/usr/local/bin/pi",
        "/opt/homebrew/bin/pi",
    ] {
        if std::path::Path::new(candidate).exists() {
            return Ok(candidate.to_string());
        }
    }

    Err("Pi binary not found. Install it with: npm install -g @mariozechner/pi-coding-agent".to_string())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderInfo {
    id: String,
    label: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelInfo {
    id: String,
    label: String,
}

fn get_known_providers() -> Vec<ProviderInfo> {
    [
        ("openrouter", "OpenRouter"),
        ("nestor", "Nestor (Tinkoff)"),
        ("anthropic", "Anthropic"),
        ("openai", "OpenAI"),
        ("google", "Google"),
        ("ollama", "Ollama (local)"),
    ]
    .into_iter()
    .map(|(id, label)| ProviderInfo {
        id: id.to_string(),
        label: label.to_string(),
    })
    .collect()
}

fn get_known_models(provider: &str) -> Vec<ModelInfo> {
    let pairs: &[(&str, &str)] = match provider {
        "openrouter" => &[
            ("openrouter/anthropic/claude-sonnet-4-6", "Claude Sonnet 4.6"),
            ("openrouter/anthropic/claude-haiku-4-5", "Claude Haiku 4.5"),
            ("openrouter/anthropic/claude-opus-4-6", "Claude Opus 4.6"),
            ("openrouter/openai/gpt-5.4", "GPT 5.4"),
            ("openrouter/openai/gpt-5.3-codex", "GPT 5.3 Codex"),
            ("openrouter/openai/o3", "o3"),
            ("openrouter/google/gemini-2.5-pro", "Gemini 2.5 Pro"),
            ("openrouter/google/gemini-2.5-flash", "Gemini 2.5 Flash"),
            ("openrouter/deepseek/deepseek-chat-v3", "DeepSeek V3"),
            ("openrouter/qwen/qwen3-235b-a22b", "Qwen 3 235B"),
        ],
        "nestor" => &[
            ("nestor/tgpt/qwen35-397b-a17b-fp8", "Qwen 3.5 397B"),
            ("nestor/tgpt/qwen3-next-80b-a3b-instruct", "Qwen 3 Next 80B"),
            ("nestor/tgpt/gpt-oss-120b", "GPT-OSS 120B"),
        ],
        "anthropic" => &[
            ("anthropic/claude-sonnet-4-6", "Claude Sonnet 4.6"),
            ("anthropic/claude-haiku-4-5", "Claude Haiku 4.5"),
            ("anthropic/claude-opus-4-6", "Claude Opus 4.6"),
        ],
        "openai" => &[
            ("openai/gpt-5.4", "GPT 5.4"),
            ("openai/gpt-5.3-codex", "GPT 5.3 Codex"),
            ("openai/o3", "o3"),
        ],
        "google" => &[
            ("google/gemini-2.5-pro", "Gemini 2.5 Pro"),
            ("google/gemini-2.5-flash", "Gemini 2.5 Flash"),
        ],
        _ => &[],
    };
    pairs
        .iter()
        .map(|(id, label)| ModelInfo {
            id: id.to_string(),
            label: label.to_string(),
        })
        .collect()
}

#[tauri::command]
fn list_providers() -> Vec<ProviderInfo> {
    get_known_providers()
}

#[tauri::command]
fn list_models(provider: String) -> Vec<ModelInfo> {
    get_known_models(&provider)
}

#[derive(Clone, Serialize, Deserialize)]
struct ConfigData {
    provider: String,
    models: ModelConfig,
}

#[derive(Clone, Serialize, Deserialize)]
struct ModelConfig {
    orchestrator: String,
    worker: String,
    reviewer: String,
    explorer: String,
    tester: String,
    #[serde(rename = "promptEngineer")]
    prompt_engineer: String,
}

#[tauri::command]
fn get_config(config_path: String) -> Result<ConfigData, String> {
    let contents =
        std::fs::read_to_string(&config_path).map_err(|e| format!("Failed to read {config_path}: {e}"))?;
    let config: ConfigData =
        serde_yaml::from_str(&contents).map_err(|e| format!("Failed to parse YAML: {e}"))?;
    Ok(config)
}

#[tauri::command]
fn save_config(config_path: String, config: ConfigData) -> Result<(), String> {
    let yaml =
        serde_yaml::to_string(&config).map_err(|e| format!("Failed to serialize config: {e}"))?;
    std::fs::write(&config_path, yaml).map_err(|e| format!("Failed to write {config_path}: {e}"))?;
    Ok(())
}

#[tauri::command]
fn get_active_config_name(pi_dir: String) -> Result<String, String> {
    let path = std::path::Path::new(&pi_dir).join("agentic-kit.json");
    let contents =
        std::fs::read_to_string(&path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&contents).map_err(|e| format!("Failed to parse JSON: {e}"))?;
    json.get("config")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "No 'config' field in agentic-kit.json".to_string())
}

#[tauri::command]
fn set_active_config_name(pi_dir: String, name: String) -> Result<(), String> {
    let path = std::path::Path::new(&pi_dir).join("agentic-kit.json");
    let contents =
        std::fs::read_to_string(&path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    let mut json: serde_json::Value =
        serde_json::from_str(&contents).map_err(|e| format!("Failed to parse JSON: {e}"))?;
    json.as_object_mut()
        .ok_or_else(|| "agentic-kit.json is not an object".to_string())?
        .insert("config".to_string(), serde_json::Value::String(name));
    let out = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Failed to serialize JSON: {e}"))?;
    std::fs::write(&path, out).map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
    Ok(())
}

#[tauri::command]
fn list_system_fonts() -> Vec<String> {
    let source = SystemSource::new();
    let mut families: Vec<String> = source
        .all_families()
        .unwrap_or_default()
        .into_iter()
        .filter(|name| !name.starts_with('.'))
        .collect();
    families.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    families
}

pub fn run() {
    let state = AppState {
        agents: Arc::new(Mutex::new(HashMap::new())),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            spawn_agent,
            list_agents,
            write_input,
            resize_pty,
            kill_agent,
            get_pi_binary_path,
            get_config,
            save_config,
            get_active_config_name,
            set_active_config_name,
            get_preferences,
            save_preferences,
            check_pi_config,
            install_pi_config,
            get_project_config,
            save_project_config,
            list_providers,
            list_models,
            list_system_fonts,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                let agents_arc = {
                    let app_state: tauri::State<'_, AppState> = window.state();
                    app_state.agents.clone()
                };
                // Collect agents to kill, then drop the lock before cleanup
                let to_kill: Vec<Agent> = {
                    let mut guard = match agents_arc.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    let ids: Vec<String> = guard.keys().cloned().collect();
                    ids.into_iter().filter_map(|id| guard.remove(&id)).collect()
                };
                for agent in to_kill {
                    if let Ok(mut child) = agent.child.lock() {
                        let _ = child.kill();
                    }
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
