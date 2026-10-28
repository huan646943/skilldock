use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use jsonc_parser::cst::{CstInputValue, CstLeafNode, CstNode, CstRootNode};
use jsonc_parser::ParseOptions;
use serde_json::Value as JsonValue;

use crate::library::resolve_command_in_path;
use crate::models::{PluginComponentSummary, PluginProbeResult, PluginScopeSummary, PluginSummary};
use crate::workspace::{self, opencode_config_path_for_home};

const OPENCODE_PLUGIN_DEPENDENCIES: [&str; 2] = ["@opencode-ai/plugin", "@opencode/plugin"];
const OPENCODE_NPM_DIR: &str = "opencode-npm";

#[derive(Clone, Debug)]
pub struct PackagePluginInfo {
    pub package_name: String,
    pub version: String,
    pub description: String,
    pub opencode_npm: bool,
    pub lockfile_hosts: Vec<String>,
}

impl PackagePluginInfo {
    pub fn compatible_hosts(&self) -> Vec<String> {
        let mut hosts = self.lockfile_hosts.clone();
        if self.opencode_npm && !hosts.iter().any(|host| host == "opencode") {
            hosts.push("opencode".to_string());
        }
        hosts
    }
}

pub fn inspect_package_dir(root: &Path) -> Option<PackagePluginInfo> {
    let content = fs::read_to_string(root.join("package.json")).ok()?;
    let package = serde_json::from_str::<JsonValue>(&content).ok()?;
    inspect_package_value(&package)
}

pub fn inspect_package_value(package: &JsonValue) -> Option<PackagePluginInfo> {
    let package_name = package
        .get("name")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let version = package
        .get("version")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let description = package
        .get("description")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let opencode_npm = has_opencode_plugin_keyword(package) || depends_on_opencode_plugin(package);
    let has_omp = is_non_empty_block(package.get("omp"));
    let has_pi = is_non_empty_block(package.get("pi"));
    let lockfile_hosts = lockfile_hosts(has_omp, has_pi);
    if !opencode_npm && lockfile_hosts.is_empty() {
        return None;
    }
    Some(PackagePluginInfo {
        package_name,
        version,
        description,
        opencode_npm,
        lockfile_hosts,
    })
}

pub fn prefers_opencode_config(probe: &PluginProbeResult, source_root: &Path) -> bool {
    if probe.install_strategy == "opencode-plugin-link"
        || has_opencode_plugin_entrypoint(source_root)
    {
        return false;
    }
    probe.install_strategy == "opencode-config-plugin"
        || inspect_package_dir(source_root).is_some_and(|info| info.opencode_npm)
}

pub fn ensure_lockfile_host_available(host_tool: &str) -> Result<(), String> {
    let marker = match host_tool {
        "omp" => ".omp",
        "pi" => ".pi",
        _ => return Err(format!("不支持的插件宿主: {host_tool}")),
    };
    let label = if host_tool == "omp" { "OMP" } else { "Pi" };
    if workspace::home_dir_option().is_some_and(|home| home.join(marker).is_dir())
        || resolve_command_in_path(host_tool).is_some()
    {
        return Ok(());
    }
    Err(format!("未检测到 {label}，安装该插件前请先安装 {label}。"))
}

pub fn install_opencode_config_plugin(
    home_dir: &Path,
    probe: &PluginProbeResult,
) -> Result<PathBuf, String> {
    let info = opencode_package_info(probe)?;
    let metadata_root = write_opencode_npm_metadata(home_dir, &info, &probe.source_url)?;
    set_opencode_plugin_listed(home_dir, &info.package_name, true)?;
    Ok(metadata_root)
}

pub fn scan_opencode_config_plugins() -> Vec<PluginSummary> {
    let Some(home_dir) = workspace::home_dir_option() else {
        return Vec::new();
    };
    let listed = read_opencode_plugin_names(&opencode_config_path_for_home(&home_dir));
    let mut package_names = listed.clone();
    if let Ok(entries) = fs::read_dir(opencode_npm_root(&home_dir)) {
        for entry in entries.flatten() {
            let metadata = read_opencode_npm_metadata(&entry.path());
            if let Some(metadata) = metadata {
                if !package_names
                    .iter()
                    .any(|name| name == &metadata.package_name)
                {
                    package_names.push(metadata.package_name);
                }
            }
        }
    }

    package_names
        .into_iter()
        .filter_map(|package_name| {
            let enabled = listed.iter().any(|name| name == &package_name);
            let metadata_root = ensure_recorded_opencode_metadata(&home_dir, &package_name).ok()?;
            let metadata =
                read_opencode_npm_metadata(&metadata_root).unwrap_or(OpenCodeNpmMetadata {
                    package_name: package_name.clone(),
                    source_url: String::new(),
                    version: String::new(),
                    description: String::new(),
                });
            Some(config_plugin_summary(&metadata_root, &metadata, enabled))
        })
        .collect()
}

pub fn set_opencode_config_enabled(
    root_path: &str,
    enabled: bool,
) -> Result<PluginSummary, String> {
    let home_dir = workspace::home_dir()?;
    let package_name = opencode_package_name_at(&home_dir, Path::new(root_path))?;
    set_opencode_plugin_listed(&home_dir, &package_name, enabled)?;
    let metadata_root = ensure_recorded_opencode_metadata(&home_dir, &package_name)?;
    scan_opencode_config_plugins()
        .into_iter()
        .find(|plugin| plugin.root_path == metadata_root.to_string_lossy())
        .ok_or_else(|| "OpenCode npm 插件状态已写入，但重新读取失败".to_string())
}

pub fn delete_opencode_config_plugin(root_path: &str) -> Result<(), String> {
    let home_dir = workspace::home_dir()?;
    let metadata_root = PathBuf::from(root_path);
    if let Ok(package_name) = opencode_package_name_at(&home_dir, &metadata_root) {
        set_opencode_plugin_listed(&home_dir, &package_name, false)?;
    }
    if is_opencode_npm_root(&metadata_root) && fs::symlink_metadata(&metadata_root).is_ok() {
        remove_path(&metadata_root)?;
    }
    Ok(())
}

pub fn is_opencode_npm_root(path: &Path) -> bool {
    let mut saw_workspace = false;
    let mut saw_npm_dir = false;
    for component in path.components() {
        let name = component.as_os_str();
        if name == ".skilldock" {
            saw_workspace = true;
        }
        if name == OPENCODE_NPM_DIR {
            saw_npm_dir = true;
        }
    }
    saw_workspace && saw_npm_dir
}

pub fn install_lockfile_plugin(
    home_dir: &Path,
    source_root: &Path,
    host_tool: &str,
) -> Result<PathBuf, String> {
    let info = inspect_package_dir(source_root)
        .ok_or_else(|| format!("目录不是有效的 {host_tool} 插件: {}", source_root.display()))?;
    if info.package_name.trim().is_empty() {
        return Err("插件 package.json 缺少 name".into());
    }
    if !info.lockfile_hosts.iter().any(|host| host == host_tool) {
        return Err(format!("{} 不能安装到 {host_tool}", info.package_name));
    }
    let plugins_root = lockfile_plugins_root(home_dir, host_tool);
    fs::create_dir_all(&plugins_root).map_err(|error| {
        format!(
            "创建 {host_tool} 插件目录失败（{}）: {error}",
            plugins_root.display()
        )
    })?;
    let link_path = node_modules_package_path(&plugins_root, &info.package_name);
    symlink_dir(source_root, &link_path)?;
    let version = if info.version.is_empty() {
        "*".to_string()
    } else {
        info.version.clone()
    };
    upsert_dependency(
        &plugins_root.join("package.json"),
        &info.package_name,
        &version,
    )?;
    upsert_lock_entry(
        &lockfile_path(home_dir, host_tool),
        &info.package_name,
        &version,
        true,
    )?;
    set_json_array_value(
        &agent_settings_path(home_dir, host_tool),
        "packages",
        &source_root.to_string_lossy(),
        true,
    )?;
    Ok(link_path)
}

pub fn scan_lockfile_plugins(host_tool: &str) -> Vec<PluginSummary> {
    let Some(home_dir) = workspace::home_dir_option() else {
        return Vec::new();
    };
    let plugins_root = lockfile_plugins_root(&home_dir, host_tool);
    if host_tool == "pi" && !plugins_root.is_dir() {
        return Vec::new();
    }
    if !plugins_root.exists() {
        return Vec::new();
    }
    let dependencies = read_dependency_names(&plugins_root.join("package.json"));
    let lock_path = lockfile_path(&home_dir, host_tool);
    let lock_entries = read_lock_entries(&lock_path);
    let mut names = dependencies;
    for name in lock_entries.keys() {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.clone());
        }
    }
    names.sort();
    names
        .into_iter()
        .map(|package_name| {
            let enabled = lock_entries
                .get(&package_name)
                .and_then(|entry| entry.enabled)
                .unwrap_or(true);
            let link_path = node_modules_package_path(&plugins_root, &package_name);
            let source_root = fs::canonicalize(&link_path).unwrap_or(link_path);
            let info = inspect_package_dir(&source_root);
            let description = official_manifest_description(&source_root).unwrap_or_else(|| {
                info.as_ref()
                    .map(|info| info.description.clone())
                    .unwrap_or_default()
            });
            let version = lock_entries
                .get(&package_name)
                .map(|entry| entry.version.clone())
                .filter(|version| !version.is_empty())
                .or_else(|| info.as_ref().map(|info| info.version.clone()))
                .unwrap_or_default();
            let aggregation_name =
                official_manifest_name(&source_root).unwrap_or_else(|| package_name.clone());
            lockfile_plugin_summary(LockfileSummaryArgs {
                host_tool: host_tool.to_string(),
                package_name,
                aggregation_name,
                description,
                version,
                root: source_root,
                plugins_root: plugins_root.clone(),
                enabled,
            })
        })
        .collect()
}

pub fn set_lockfile_plugin_enabled(
    host_tool: &str,
    root_path: &str,
    enabled: bool,
) -> Result<PluginSummary, String> {
    let home_dir = workspace::home_dir()?;
    let package_name = package_name_from_root(host_tool, &home_dir, Path::new(root_path))?;
    let version = read_lock_entries(&lockfile_path(&home_dir, host_tool))
        .get(&package_name)
        .map(|entry| entry.version.clone())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| "*".to_string());
    sync_agent_package_enabled(&home_dir, host_tool, &package_name, enabled)?;
    upsert_lock_entry(
        &lockfile_path(&home_dir, host_tool),
        &package_name,
        &version,
        enabled,
    )?;
    scan_lockfile_plugins(host_tool)
        .into_iter()
        .find(|plugin| plugin.manifest_name == package_name || plugin.name == package_name)
        .ok_or_else(|| format!("{host_tool} 插件状态已写入，但重新读取失败"))
}

pub fn remove_lockfile_install(
    host_tool: &str,
    root_path: &str,
) -> Result<Option<PathBuf>, String> {
    let home_dir = workspace::home_dir()?;
    let package_name = package_name_from_root(host_tool, &home_dir, Path::new(root_path))?;
    let plugins_root = lockfile_plugins_root(&home_dir, host_tool);
    let link_path = node_modules_package_path(&plugins_root, &package_name);
    let managed_root = managed_package_root_from_link(&home_dir, &link_path);
    remove_pi_package_entries(
        &agent_settings_path(&home_dir, host_tool),
        &package_name,
        managed_root.as_deref(),
    )?;
    if fs::symlink_metadata(&link_path).is_ok() {
        remove_path(&link_path)?;
    }
    remove_dependency(&plugins_root.join("package.json"), &package_name)?;
    remove_lock_entry(&lockfile_path(&home_dir, host_tool), &package_name)?;
    Ok(managed_root)
}

pub fn append_extra_components(
    root: &Path,
    owner_plugin_id: &str,
    components: &mut Vec<PluginComponentSummary>,
) {
    append_directory_components(
        root,
        root.join("extensions"),
        "hook",
        owner_plugin_id,
        components,
    );
    for relative in [".opencode/mcp.json", "mcp/mcp.json", ".mcp/mcp.json"] {
        let path = root.join(relative);
        if path.is_file()
            && !components
                .iter()
                .any(|component| component.package_item_id == relative)
        {
            components.push(component_summary(
                relative,
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(relative),
                "mcp",
                owner_plugin_id,
            ));
        }
    }
    let Ok(content) = fs::read_to_string(root.join("package.json")) else {
        return;
    };
    let Ok(package) = serde_json::from_str::<JsonValue>(&content) else {
        return;
    };
    let mut declared_paths = Vec::new();
    for key in ["pi", "omp"] {
        if let Some(block) = package.get(key) {
            collect_relative_paths(block, &mut declared_paths);
        }
    }
    for relative in declared_paths {
        let full_path = root.join(&relative);
        if !full_path.exists() {
            continue;
        }
        if append_nested_skill_components(root, &full_path, owner_plugin_id, components) {
            continue;
        }
        if components.iter().any(|component| {
            component.package_item_id == relative
                || component
                    .package_item_id
                    .starts_with(&format!("{relative}/"))
        }) {
            continue;
        }
        let name = Path::new(&relative)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(relative.as_str());
        let mut component = component_summary(
            &relative,
            name,
            asset_type_for_path(&relative),
            owner_plugin_id,
        );
        if component.asset_type == "skill" {
            component.description = read_skill_component_description(&full_path);
        }
        components.push(component);
    }
}

fn append_nested_skill_components(
    root: &Path,
    directory: &Path,
    owner_plugin_id: &str,
    components: &mut Vec<PluginComponentSummary>,
) -> bool {
    if !directory.is_dir() {
        return false;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };
    let mut skill_dirs = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.join("SKILL.md").is_file())
        .collect::<Vec<_>>();
    if skill_dirs.is_empty() {
        return false;
    }
    skill_dirs.sort();
    for skill_dir in skill_dirs {
        let Ok(relative_path) = skill_dir.strip_prefix(root) else {
            continue;
        };
        let relative = relative_path.to_string_lossy().replace('\\', "/");
        if components
            .iter()
            .any(|component| component.package_item_id == relative)
        {
            continue;
        }
        let name = skill_dir
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(relative.as_str());
        let mut component = component_summary(&relative, name, "skill", owner_plugin_id);
        component.description = read_skill_component_description(&skill_dir);
        components.push(component);
    }
    true
}

fn read_skill_component_description(skill_dir: &Path) -> String {
    let Ok(content) = fs::read_to_string(skill_dir.join("SKILL.md")) else {
        return String::new();
    };
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return String::new();
    }
    for line in trimmed.lines().skip(1) {
        let line = line.trim();
        if line == "---" {
            break;
        }
        let Some(value) = line.strip_prefix("description:") else {
            continue;
        };
        let value = value
            .trim()
            .trim_matches(|ch| ch == '"' || ch == '\'')
            .to_string();
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

fn lockfile_hosts(has_omp: bool, has_pi: bool) -> Vec<String> {
    match (has_omp, has_pi) {
        (false, false) => Vec::new(),
        (true, false) => vec!["omp".to_string()],
        (_, true) => vec!["pi".to_string(), "omp".to_string()],
    }
}

fn has_opencode_plugin_keyword(package: &JsonValue) -> bool {
    package
        .get("keywords")
        .and_then(JsonValue::as_array)
        .is_some_and(|keywords| {
            keywords.iter().any(|keyword| {
                keyword
                    .as_str()
                    .is_some_and(|value| value.eq_ignore_ascii_case("opencode-plugin"))
            })
        })
}

fn depends_on_opencode_plugin(package: &JsonValue) -> bool {
    [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ]
    .iter()
    .any(|field| {
        package
            .get(*field)
            .and_then(JsonValue::as_object)
            .is_some_and(|dependencies| {
                OPENCODE_PLUGIN_DEPENDENCIES
                    .iter()
                    .any(|name| dependencies.contains_key(*name))
            })
    })
}

fn is_non_empty_block(value: Option<&JsonValue>) -> bool {
    match value {
        Some(JsonValue::Object(entries)) => !entries.is_empty(),
        Some(JsonValue::Array(entries)) => !entries.is_empty(),
        Some(JsonValue::String(value)) => !value.trim().is_empty(),
        _ => false,
    }
}

fn has_opencode_plugin_entrypoint(root: &Path) -> bool {
    let Ok(entries) = fs::read_dir(root.join(".opencode/plugins")) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        path.is_file()
            && matches!(
                path.extension()
                    .and_then(|value| value.to_str())
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("js" | "ts")
            )
    })
}

fn opencode_package_info(probe: &PluginProbeResult) -> Result<PackagePluginInfo, String> {
    let local_root = Path::new(&probe.plugin_root);
    if local_root.is_dir() {
        if let Some(info) = inspect_package_dir(local_root).filter(|info| info.opencode_npm) {
            if info.package_name.is_empty() {
                return Err("OpenCode npm 插件 package.json 缺少 name".into());
            }
            return Ok(info);
        }
    }
    let package_name = [probe.manifest_name.as_str(), probe.name.as_str()]
        .into_iter()
        .map(str::trim)
        .find(|name| !name.is_empty() && !name.contains(' '))
        .ok_or_else(|| "无法确定 OpenCode npm 插件包名".to_string())?;
    Ok(PackagePluginInfo {
        package_name: package_name.to_string(),
        version: String::new(),
        description: probe.description.clone(),
        opencode_npm: true,
        lockfile_hosts: Vec::new(),
    })
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenCodeNpmMetadata {
    package_name: String,
    #[serde(default)]
    source_url: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    description: String,
}

fn opencode_npm_root(home_dir: &Path) -> PathBuf {
    home_dir.join(".skilldock").join(OPENCODE_NPM_DIR)
}

fn opencode_metadata_dir(home_dir: &Path, package_name: &str) -> PathBuf {
    opencode_npm_root(home_dir).join(slugify(package_name))
}

fn write_opencode_npm_metadata(
    home_dir: &Path,
    info: &PackagePluginInfo,
    source_url: &str,
) -> Result<PathBuf, String> {
    let metadata_root = opencode_metadata_dir(home_dir, &info.package_name);
    fs::create_dir_all(&metadata_root).map_err(|error| {
        format!(
            "创建 OpenCode npm 插件记录失败（{}）: {error}",
            metadata_root.display()
        )
    })?;
    let metadata = OpenCodeNpmMetadata {
        package_name: info.package_name.clone(),
        source_url: source_url.trim().to_string(),
        version: info.version.clone(),
        description: info.description.clone(),
    };
    let content = serde_json::to_string_pretty(&metadata)
        .map_err(|error| format!("序列化 OpenCode npm 插件记录失败: {error}"))?;
    fs::write(metadata_root.join("metadata.json"), format!("{content}\n")).map_err(|error| {
        format!(
            "写入 OpenCode npm 插件记录失败（{}）: {error}",
            metadata_root.display()
        )
    })?;
    Ok(metadata_root)
}

fn ensure_recorded_opencode_metadata(
    home_dir: &Path,
    package_name: &str,
) -> Result<PathBuf, String> {
    let metadata_root = opencode_metadata_dir(home_dir, package_name);
    if read_opencode_npm_metadata(&metadata_root).is_some() {
        return Ok(metadata_root);
    }
    write_opencode_npm_metadata(
        home_dir,
        &PackagePluginInfo {
            package_name: package_name.to_string(),
            version: String::new(),
            description: String::new(),
            opencode_npm: true,
            lockfile_hosts: Vec::new(),
        },
        "",
    )
}

fn read_opencode_npm_metadata(metadata_root: &Path) -> Option<OpenCodeNpmMetadata> {
    let content = fs::read_to_string(metadata_root.join("metadata.json")).ok()?;
    serde_json::from_str(&content).ok()
}

fn opencode_package_name_at(home_dir: &Path, root_path: &Path) -> Result<String, String> {
    if let Some(metadata) = read_opencode_npm_metadata(root_path) {
        return Ok(metadata.package_name);
    }
    if let Ok(entries) = fs::read_dir(opencode_npm_root(home_dir)) {
        for entry in entries.flatten() {
            if paths_match(&entry.path(), root_path) {
                if let Some(metadata) = read_opencode_npm_metadata(&entry.path()) {
                    return Ok(metadata.package_name);
                }
            }
        }
    }
    Err("无法识别 OpenCode npm 插件包名".into())
}

fn set_opencode_plugin_listed(
    home_dir: &Path,
    package_name: &str,
    listed: bool,
) -> Result<(), String> {
    set_json_array_value(
        &opencode_config_path_for_home(home_dir),
        "plugin",
        package_name,
        listed,
    )
}

fn read_opencode_plugin_names(path: &Path) -> Vec<String> {
    read_json_string_array(path, "plugin").unwrap_or_default()
}

fn config_plugin_summary(
    metadata_root: &Path,
    metadata: &OpenCodeNpmMetadata,
    enabled: bool,
) -> PluginSummary {
    let enabled_state = if enabled { "enabled" } else { "disabled" };
    plugin_summary(PluginSummaryArgs {
        host_tool: "opencode",
        package_name: &metadata.package_name,
        aggregation_name: &metadata.package_name,
        description: &metadata.description,
        version: &metadata.version,
        root: metadata_root,
        display_root: metadata_root,
        source_url: &metadata.source_url,
        source_label: "npm",
        enabled_state,
        components: Vec::new(),
    })
}

struct LockEntry {
    version: String,
    enabled: Option<bool>,
}

fn lockfile_plugins_root(home_dir: &Path, host_tool: &str) -> PathBuf {
    home_dir.join(format!(".{host_tool}")).join("plugins")
}

fn agent_settings_path(home_dir: &Path, host_tool: &str) -> PathBuf {
    home_dir
        .join(format!(".{host_tool}"))
        .join("agent/settings.json")
}

fn sync_agent_package_enabled(
    home_dir: &Path,
    host_tool: &str,
    package_name: &str,
    enabled: bool,
) -> Result<(), String> {
    let settings_path = agent_settings_path(home_dir, host_tool);
    let link_path =
        node_modules_package_path(&lockfile_plugins_root(home_dir, host_tool), package_name);
    let managed_root = managed_package_root_from_link(home_dir, &link_path);
    if enabled {
        let source_root = managed_root
            .ok_or_else(|| format!("找不到 {host_tool} 插件目录，无法启用 {package_name}"))?;
        return set_json_array_value(
            &settings_path,
            "packages",
            &source_root.to_string_lossy(),
            true,
        );
    }
    remove_pi_package_entries(&settings_path, package_name, managed_root.as_deref())
}

fn lockfile_path(home_dir: &Path, host_tool: &str) -> PathBuf {
    lockfile_plugins_root(home_dir, host_tool).join(format!("{host_tool}-plugins.lock.json"))
}

fn node_modules_package_path(plugins_root: &Path, package_name: &str) -> PathBuf {
    let mut path = plugins_root.join("node_modules");
    for segment in package_name.split('/') {
        path.push(segment);
    }
    path
}

fn read_dependency_names(path: &Path) -> Vec<String> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(document) = serde_json::from_str::<JsonValue>(&content) else {
        return Vec::new();
    };
    document
        .get("dependencies")
        .and_then(JsonValue::as_object)
        .map(|dependencies| dependencies.keys().cloned().collect())
        .unwrap_or_default()
}

fn read_lock_entries(path: &Path) -> std::collections::BTreeMap<String, LockEntry> {
    let Ok(content) = fs::read_to_string(path) else {
        return std::collections::BTreeMap::new();
    };
    let Ok(document) = serde_json::from_str::<JsonValue>(&content) else {
        return std::collections::BTreeMap::new();
    };
    let Some(plugins) = document.get("plugins").and_then(JsonValue::as_object) else {
        return std::collections::BTreeMap::new();
    };
    plugins
        .iter()
        .map(|(name, entry)| {
            (
                name.clone(),
                LockEntry {
                    version: entry
                        .get("version")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    enabled: entry.get("enabled").and_then(JsonValue::as_bool),
                },
            )
        })
        .collect()
}

fn upsert_dependency(path: &Path, package_name: &str, version: &str) -> Result<(), String> {
    let mut document = read_json_object_or_empty(path)?;
    let dependencies = object_entry(&mut document, "dependencies")?;
    dependencies.insert(
        package_name.to_string(),
        JsonValue::String(version.to_string()),
    );
    write_pretty_json(path, &document)
}

fn remove_dependency(path: &Path, package_name: &str) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let mut document = read_json_object_or_empty(path)?;
    if let Some(dependencies) = document
        .get_mut("dependencies")
        .and_then(JsonValue::as_object_mut)
    {
        dependencies.remove(package_name);
    }
    write_pretty_json(path, &document)
}

fn upsert_lock_entry(
    path: &Path,
    package_name: &str,
    version: &str,
    enabled: bool,
) -> Result<(), String> {
    let mut document = read_json_object_or_empty(path)?;
    let plugins = object_entry(&mut document, "plugins")?;
    let entry = plugins
        .entry(package_name.to_string())
        .or_insert_with(|| JsonValue::Object(serde_json::Map::new()));
    let entry_object = entry
        .as_object_mut()
        .ok_or_else(|| format!("{package_name} 的 lock 记录不是对象"))?;
    if !entry_object.contains_key("version") {
        entry_object.insert(
            "version".to_string(),
            JsonValue::String(version.to_string()),
        );
    }
    entry_object.insert("enabled".to_string(), JsonValue::Bool(enabled));
    write_pretty_json(path, &document)
}

fn object_entry<'a>(
    document: &'a mut JsonValue,
    key: &str,
) -> Result<&'a mut serde_json::Map<String, JsonValue>, String> {
    if !document.is_object() {
        *document = JsonValue::Object(serde_json::Map::new());
    }
    let object = document
        .as_object_mut()
        .ok_or_else(|| format!("{key} 配置不是对象"))?;
    let entry = object
        .entry(key.to_string())
        .or_insert_with(|| JsonValue::Object(serde_json::Map::new()));
    entry
        .as_object_mut()
        .ok_or_else(|| format!("{key} 不是对象"))
}

fn remove_lock_entry(path: &Path, package_name: &str) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let mut document = read_json_object_or_empty(path)?;
    if let Some(plugins) = document
        .get_mut("plugins")
        .and_then(JsonValue::as_object_mut)
    {
        plugins.remove(package_name);
    }
    write_pretty_json(path, &document)
}

fn package_name_from_root(
    host_tool: &str,
    home_dir: &Path,
    root_path: &Path,
) -> Result<String, String> {
    if let Some(package_name) = npm_package_name_from_node_modules(root_path) {
        return Ok(package_name);
    }
    if let Some(info) = inspect_package_dir(root_path).filter(|info| !info.package_name.is_empty())
    {
        return Ok(info.package_name);
    }
    let canonical = fs::canonicalize(root_path).unwrap_or_else(|_| root_path.to_path_buf());
    scan_lockfile_plugins(host_tool)
        .into_iter()
        .find(|plugin| {
            plugin.root_path == root_path.to_string_lossy()
                || plugin.root_path == canonical.to_string_lossy()
        })
        .map(|plugin| plugin.name)
        .or_else(|| {
            let _ = home_dir;
            None
        })
        .ok_or_else(|| format!("无法识别 {host_tool} 插件包名: {}", root_path.display()))
}

fn npm_package_name_from_node_modules(path: &Path) -> Option<String> {
    let components = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>();
    let index = components
        .iter()
        .position(|component| component == "node_modules")?;
    let rest = &components[index + 1..];
    match rest {
        [name] if !name.starts_with('@') => Some(name.clone()),
        [scope, name] if scope.starts_with('@') => Some(format!("{scope}/{name}")),
        _ => None,
    }
}

fn official_manifest_description(root: &Path) -> Option<String> {
    official_manifest_string(root, "description")
}

fn official_manifest_name(root: &Path) -> Option<String> {
    official_manifest_string(root, "name")
}

fn official_manifest_string(root: &Path, field: &str) -> Option<String> {
    for relative in [
        ".claude-plugin/plugin.json",
        ".cursor-plugin/plugin.json",
        ".codex-plugin/plugin.json",
        ".opencode-plugin/plugin.json",
        "plugin.json",
    ] {
        let Ok(content) = fs::read_to_string(root.join(relative)) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<JsonValue>(&content) else {
            continue;
        };
        let value = manifest
            .get(field)
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

fn managed_package_root_from_link(home_dir: &Path, link_path: &Path) -> Option<PathBuf> {
    let target = resolved_link_target(link_path)?;
    let managed_root = home_dir.join(".skilldock/plugins");
    let canonical_managed_root = fs::canonicalize(&managed_root).unwrap_or(managed_root);
    let relative = target
        .strip_prefix(&canonical_managed_root)
        .or_else(|_| target.strip_prefix(home_dir.join(".skilldock/plugins")))
        .ok()?;
    let package_name = relative.components().next()?.as_os_str();
    Some(canonical_managed_root.join(package_name))
}

fn resolved_link_target(link_path: &Path) -> Option<PathBuf> {
    if let Ok(target) = fs::canonicalize(link_path) {
        return Some(target);
    }
    let link_target = fs::read_link(link_path).ok()?;
    if link_target.is_absolute() {
        Some(link_target)
    } else {
        Some(link_path.parent()?.join(link_target))
    }
}

struct LockfileSummaryArgs {
    host_tool: String,
    package_name: String,
    aggregation_name: String,
    description: String,
    version: String,
    root: PathBuf,
    plugins_root: PathBuf,
    enabled: bool,
}

fn lockfile_plugin_summary(args: LockfileSummaryArgs) -> PluginSummary {
    let owner_id = format!("{}:{}", args.host_tool, slugify(&args.aggregation_name));
    let components = crate::plugin_manager::collect_asset_components(&args.root, &owner_id);
    let enabled_state = if args.enabled { "enabled" } else { "disabled" };
    let source = crate::plugin_manager::plugin_source_record(&args.root);
    let mut summary = plugin_summary(PluginSummaryArgs {
        host_tool: &args.host_tool,
        package_name: &args.package_name,
        aggregation_name: &args.aggregation_name,
        description: &args.description,
        version: &args.version,
        root: &args.root,
        display_root: &args.plugins_root,
        source_url: &source.source_url,
        source_label: &args.host_tool,
        enabled_state,
        components,
    });
    if !summary.source_url.trim().is_empty() {
        if !source.source_type.trim().is_empty() {
            summary.source_type = source.source_type;
        }
        summary.source_ref = source.source_ref;
        summary.source_revision = source.source_revision;
        summary.is_git_repo = source.is_git_repo || summary.source_type == "git";
        if summary.is_git_repo {
            summary.update_strategy = "git".to_string();
        }
    }
    if let Some(modified_at) = package_modified_at(&args.root) {
        summary.installed_at = modified_at.clone();
        summary.updated_at = modified_at.clone();
        summary.local_updated_at = modified_at;
    }
    summary
}

fn package_modified_at(root: &Path) -> Option<String> {
    ["package.json", "plugin.json"]
        .into_iter()
        .filter_map(|relative| fs::metadata(root.join(relative)).ok())
        .filter_map(|metadata| metadata.modified().ok())
        .filter_map(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .max()
        .map(|modified| modified.to_string())
}

struct PluginSummaryArgs<'a> {
    host_tool: &'a str,
    package_name: &'a str,
    aggregation_name: &'a str,
    description: &'a str,
    version: &'a str,
    root: &'a Path,
    display_root: &'a Path,
    source_url: &'a str,
    source_label: &'a str,
    enabled_state: &'a str,
    components: Vec<PluginComponentSummary>,
}

fn plugin_summary(args: PluginSummaryArgs<'_>) -> PluginSummary {
    let root_path = args.root.to_string_lossy().to_string();
    let display_root = args.display_root.to_string_lossy().to_string();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().to_string())
        .unwrap_or_default();
    PluginSummary {
        id: format!("{}:{}", args.host_tool, slugify(args.aggregation_name)),
        package_id: args.package_name.to_string(),
        manifest_name: args.aggregation_name.to_string(),
        name: args.package_name.to_string(),
        description: args.description.to_string(),
        host_tool: args.host_tool.to_string(),
        related_host_tools: Vec::new(),
        configured_skill_hosts: Vec::new(),
        kind: "plugin-repo".to_string(),
        root_path: root_path.clone(),
        display_root_path: display_root.clone(),
        repo_root_path: root_path.clone(),
        plugin_relative_path: String::new(),
        manifest_path: args.root.join("package.json").to_string_lossy().to_string(),
        source_type: if args.source_url.is_empty() {
            "local".to_string()
        } else {
            "git".to_string()
        },
        source_label: args.source_label.to_string(),
        source_url: args.source_url.to_string(),
        source_ref: String::new(),
        source_revision: String::new(),
        current_version: args.version.to_string(),
        current_branch: String::new(),
        current_commit: String::new(),
        collab_status: String::new(),
        status_text: String::new(),
        is_git_repo: false,
        update_mode: "unsupported".to_string(),
        update_strategy: "none".to_string(),
        update_available: false,
        baseline_hash: String::new(),
        local_modified: false,
        local_change_count: 0,
        local_modified_source: String::new(),
        installed_at: now.clone(),
        updated_at: now.clone(),
        remote_updated_at: String::new(),
        local_updated_at: now.clone(),
        last_editor: String::new(),
        last_scanned_at: now,
        status: "ready".to_string(),
        install_state: "installed".to_string(),
        install_source: "skilldock".to_string(),
        enabled_state: args.enabled_state.to_string(),
        scopes: vec![PluginScopeSummary {
            scope_id: "user".to_string(),
            scope_label: "用户级".to_string(),
            enabled_state: args.enabled_state.to_string(),
            location: display_root,
        }],
        components: args.components,
    }
}

fn append_directory_components(
    root: &Path,
    directory: PathBuf,
    asset_type: &str,
    owner_plugin_id: &str,
    components: &mut Vec<PluginComponentSummary>,
) {
    let Ok(entries) = fs::read_dir(&directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if components
            .iter()
            .any(|component| component.package_item_id == relative)
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        components.push(component_summary(
            &relative,
            &name,
            asset_type,
            owner_plugin_id,
        ));
    }
}

fn collect_relative_paths(value: &JsonValue, paths: &mut Vec<String>) {
    match value {
        JsonValue::String(text) => {
            let trimmed = text.trim();
            if trimmed.starts_with('.') || trimmed.contains('/') {
                paths.push(trimmed.trim_start_matches("./").to_string());
            }
        }
        JsonValue::Array(items) => {
            for item in items {
                collect_relative_paths(item, paths);
            }
        }
        JsonValue::Object(entries) => {
            for item in entries.values() {
                collect_relative_paths(item, paths);
            }
        }
        _ => {}
    }
}

fn asset_type_for_path(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.contains("skill") {
        "skill"
    } else if lower.contains("mcp") {
        "mcp"
    } else if lower.contains("command") {
        "command"
    } else if lower.contains("agent") {
        "subagent"
    } else if lower.contains("rule") {
        "rule"
    } else {
        "hook"
    }
}

fn component_summary(
    relative: &str,
    name: &str,
    asset_type: &str,
    owner_plugin_id: &str,
) -> PluginComponentSummary {
    PluginComponentSummary {
        id: relative.to_string(),
        name: name.to_string(),
        description: String::new(),
        asset_type: asset_type.to_string(),
        owner_plugin_id: owner_plugin_id.to_string(),
        package_item_id: relative.to_string(),
    }
}

fn set_json_array_value(path: &Path, key: &str, value: &str, include: bool) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("创建配置目录失败（{}）: {error}", parent.display()))?;
    }
    let content = if path.is_file() {
        fs::read_to_string(path)
            .map_err(|error| format!("读取配置失败（{}）: {error}", path.display()))?
    } else {
        "{}\n".to_string()
    };
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let root = CstRootNode::parse(content, &ParseOptions::default())
        .map_err(|error| format!("解析配置失败（{}）: {error}", path.display()))?;
    let root_object = root
        .object_value_or_create()
        .ok_or_else(|| format!("{} 根节点必须是 JSON 对象", path.display()))?;
    let array = root_object
        .array_value_or_create(key)
        .ok_or_else(|| format!("{} 的 {key} 必须是字符串数组", path.display()))?;
    let existing = array
        .elements()
        .into_iter()
        .filter_map(|element| match element {
            CstNode::Leaf(CstLeafNode::StringLit(literal)) => {
                literal.decoded_value().ok().map(|text| (text, literal))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let already_present = existing.iter().any(|(text, _)| text == value);
    if include && !already_present {
        array.append(CstInputValue::String(value.to_string()));
    }
    if !include && already_present {
        for (text, literal) in existing {
            if text == value {
                literal.remove();
            }
        }
    }
    fs::write(path, root.to_string())
        .map_err(|error| format!("写入配置失败（{}）: {error}", path.display()))
}

fn remove_pi_package_entries(
    path: &Path,
    package_name: &str,
    managed_root: Option<&Path>,
) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(path)
        .map_err(|error| format!("读取 Pi 配置失败（{}）: {error}", path.display()))?;
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let root = CstRootNode::parse(content, &ParseOptions::default())
        .map_err(|error| format!("解析 Pi 配置失败（{}）: {error}", path.display()))?;
    let Some(packages) = root
        .object_value()
        .and_then(|object| object.array_value("packages"))
    else {
        return Ok(());
    };
    let entries_to_remove = packages
        .elements()
        .into_iter()
        .filter(|entry| {
            pi_package_entry_reference(entry).is_some_and(|reference| {
                package_reference_matches(&reference, package_name, managed_root)
            })
        })
        .collect::<Vec<_>>();
    if entries_to_remove.is_empty() {
        return Ok(());
    }
    for entry in entries_to_remove {
        entry.remove();
    }
    fs::write(path, root.to_string())
        .map_err(|error| format!("写入 Pi 配置失败（{}）: {error}", path.display()))
}

fn pi_package_entry_reference(entry: &CstNode) -> Option<String> {
    if let Some(literal) = entry.as_string_lit() {
        return literal.decoded_value().ok();
    }
    entry
        .as_object()?
        .get("source")?
        .value()?
        .as_string_lit()?
        .decoded_value()
        .ok()
}

fn package_reference_matches(value: &str, package_name: &str, managed_root: Option<&Path>) -> bool {
    let trimmed = value.trim();
    if trimmed == package_name
        || trimmed == format!("npm:{package_name}")
        || trimmed
            .strip_prefix("npm:")
            .and_then(|source| source.strip_prefix(package_name))
            .is_some_and(|suffix| suffix.starts_with('@'))
    {
        return true;
    }
    let Some(managed_root) = managed_root else {
        return false;
    };
    let candidate = expand_home_path(trimmed);
    paths_match(&candidate, managed_root)
}

fn expand_home_path(value: &str) -> PathBuf {
    if let Some(relative) = value.strip_prefix("~/") {
        return workspace::home_dir_option()
            .map(|home_dir| home_dir.join(relative))
            .unwrap_or_else(|| PathBuf::from(value));
    }
    PathBuf::from(value)
}

fn read_json_string_array(path: &Path, key: &str) -> Result<Vec<String>, String> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path)
        .map_err(|error| format!("读取配置失败（{}）: {error}", path.display()))?;
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let root = CstRootNode::parse(content, &ParseOptions::default())
        .map_err(|error| format!("解析配置失败（{}）: {error}", path.display()))?;
    let Some(array) = root
        .object_value()
        .and_then(|object| object.array_value(key))
    else {
        return Ok(Vec::new());
    };
    Ok(array
        .elements()
        .iter()
        .filter_map(|element| match element {
            CstNode::Leaf(CstLeafNode::StringLit(literal)) => literal.decoded_value().ok(),
            _ => None,
        })
        .collect())
}

fn read_json_object_or_empty(path: &Path) -> Result<JsonValue, String> {
    if !path.is_file() {
        return Ok(JsonValue::Object(serde_json::Map::new()));
    }
    let content = fs::read_to_string(path)
        .map_err(|error| format!("读取 JSON 失败（{}）: {error}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(JsonValue::Object(serde_json::Map::new()));
    }
    serde_json::from_str(&content)
        .map_err(|error| format!("解析 JSON 失败（{}）: {error}", path.display()))
}

fn write_pretty_json(path: &Path, value: &JsonValue) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("创建目录失败（{}）: {error}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(value)
        .map_err(|error| format!("序列化 JSON 失败（{}）: {error}", path.display()))?;
    fs::write(path, format!("{content}\n"))
        .map_err(|error| format!("写入 JSON 失败（{}）: {error}", path.display()))
}

fn symlink_dir(source_root: &Path, target_root: &Path) -> Result<(), String> {
    if let Some(parent) = target_root.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("创建插件链接目录失败（{}）: {error}", parent.display()))?;
    }
    if fs::symlink_metadata(target_root).is_ok() {
        remove_path(target_root)?;
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source_root, target_root).map_err(|error| {
            format!(
                "创建插件目录链接失败（{} -> {}）: {error}",
                source_root.display(),
                target_root.display()
            )
        })?;
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(source_root, target_root).map_err(|error| {
            format!(
                "创建插件目录链接失败（{} -> {}）: {error}",
                source_root.display(),
                target_root.display()
            )
        })?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("读取路径失败（{}）: {error}", path.display()))?;
    let result = if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(path)
    } else {
        fs::remove_dir_all(path)
    };
    result.map_err(|error| format!("删除路径失败（{}）: {error}", path.display()))
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut pending_separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if pending_separator && !slug.is_empty() {
                slug.push('-');
            }
            pending_separator = false;
            slug.push(character.to_ascii_lowercase());
        } else {
            pending_separator = true;
        }
    }
    if slug.is_empty() {
        "plugin".to_string()
    } else {
        slug
    }
}

fn paths_match(left: &Path, right: &Path) -> bool {
    fs::canonicalize(left).ok().as_ref() == fs::canonicalize(right).ok().as_ref() || left == right
}

#[cfg(test)]
mod tests {
    use super::{
        append_extra_components, delete_opencode_config_plugin, inspect_package_dir,
        install_lockfile_plugin, install_opencode_config_plugin, remove_lockfile_install,
        scan_lockfile_plugins, scan_opencode_config_plugins, set_lockfile_plugin_enabled,
        set_opencode_config_enabled,
    };
    use crate::models::PluginProbeResult;
    use crate::workspace::TEST_ENV_LOCK;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_home(label: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = env::temp_dir().join(format!("skilldock-plugin-hosts-{label}-{suffix}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp home");
        path
    }

    fn npm_probe(name: &str, root: &str) -> PluginProbeResult {
        PluginProbeResult {
            tool: "opencode".to_string(),
            compatible_host_tools: vec!["opencode".to_string()],
            kind: "plugin-repo".to_string(),
            manifest_name: name.to_string(),
            name: name.to_string(),
            description: "Demo npm plugin".to_string(),
            plugin_root: root.to_string(),
            repo_root: String::new(),
            plugin_relative_path: String::new(),
            manifest_path: String::new(),
            marketplace_manifest_path: String::new(),
            components: Vec::new(),
            source_type: "git".to_string(),
            source_url: format!("https://github.com/example/{name}"),
            source_ref: "main".to_string(),
            is_git_repo: true,
            git_root: String::new(),
            confidence: "high".to_string(),
            install_strategy: "opencode-config-plugin".to_string(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn recognizes_opencode_npm_and_pi_omp_packages() {
        let home = temp_home("inspect");
        let npm_root = home.join("npm");
        fs::create_dir_all(&npm_root).expect("npm dir");
        fs::write(
            npm_root.join("package.json"),
            r#"{"name":"oh-my-opencode-slim","keywords":["opencode-plugin"],"dependencies":{"@opencode-ai/plugin":"1.0.0"}}"#,
        )
        .expect("write npm package");
        let npm = inspect_package_dir(&npm_root).expect("npm package");
        assert!(npm.opencode_npm);
        assert!(npm.lockfile_hosts.is_empty());

        let pi_root = home.join("pi");
        fs::create_dir_all(pi_root.join("extensions")).expect("pi dir");
        fs::write(
            pi_root.join("package.json"),
            r#"{"name":"demo-pi","version":"1.2.3","description":"Pi plugin","pi":{"extensions":["./extensions"]}}"#,
        )
        .expect("write pi package");
        let pi = inspect_package_dir(&pi_root).expect("pi package");
        assert_eq!(pi.lockfile_hosts, vec!["pi", "omp"]);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn installs_scans_toggles_and_deletes_opencode_npm_plugin() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = temp_home("opencode-npm");
        let previous_home = env::var_os("HOME");
        env::set_var("HOME", &home);
        fs::create_dir_all(home.join(".config/opencode")).expect("opencode config dir");
        fs::write(
            home.join(".config/opencode/opencode.jsonc"),
            "{\n  // keep me\n  \"plugin\": []\n}\n",
        )
        .expect("write opencode config");

        let installed = install_opencode_config_plugin(
            &home,
            &npm_probe(
                "oh-my-opencode-slim",
                "/skilldock-uncloned/example/oh-my-opencode-slim",
            ),
        )
        .expect("install npm plugin");
        let config =
            fs::read_to_string(home.join(".config/opencode/opencode.jsonc")).expect("read config");
        assert!(config.contains("// keep me"));
        assert!(config.contains("oh-my-opencode-slim"));
        let enabled = scan_opencode_config_plugins();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].enabled_state, "enabled");

        let disabled = set_opencode_config_enabled(&installed.to_string_lossy(), false)
            .expect("disable npm plugin");
        assert_eq!(disabled.enabled_state, "disabled");
        let disabled_config = fs::read_to_string(home.join(".config/opencode/opencode.jsonc"))
            .expect("read disabled config");
        assert!(!disabled_config.contains("oh-my-opencode-slim"));
        assert!(disabled_config.contains("// keep me"));

        delete_opencode_config_plugin(&installed.to_string_lossy()).expect("delete npm plugin");
        assert!(scan_opencode_config_plugins().is_empty());

        match previous_home {
            Some(value) => env::set_var("HOME", value),
            None => env::remove_var("HOME"),
        }
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn installs_scans_toggles_and_deletes_omp_and_pi_plugins() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = temp_home("lockfile");
        let previous_home = env::var_os("HOME");
        env::set_var("HOME", &home);
        fs::create_dir_all(home.join(".omp")).expect("omp home");
        fs::create_dir_all(home.join(".pi")).expect("pi home");
        let source = home.join(".skilldock/plugins/demo");
        fs::create_dir_all(source.join("extensions")).expect("source");
        fs::write(source.join("extensions/index.ts"), "export {}\n").expect("extension");
        fs::write(
            source.join("package.json"),
            r#"{"name":"demo-pi","version":"0.4.0","description":"Demo Pi","pi":{"extensions":["./extensions/index.ts"]}}"#,
        )
        .expect("package");

        let omp_link = install_lockfile_plugin(&home, &source, "omp").expect("install omp");
        let pi_link = install_lockfile_plugin(&home, &source, "pi").expect("install pi");
        assert!(omp_link.join("package.json").is_file());
        assert!(pi_link.join("package.json").is_file());
        let omp_plugins = scan_lockfile_plugins("omp");
        assert_eq!(omp_plugins.len(), 1);
        assert_eq!(omp_plugins[0].name, "demo-pi");
        assert_eq!(omp_plugins[0].enabled_state, "enabled");
        assert!(omp_plugins[0]
            .components
            .iter()
            .any(|component| component.asset_type == "hook"));
        assert!(scan_lockfile_plugins("pi").len() == 1);
        let lock = fs::read_to_string(home.join(".omp/plugins/omp-plugins.lock.json"))
            .expect("read omp lock");
        assert!(lock.contains("\"enabled\": true"));
        let settings = fs::read_to_string(home.join(".pi/agent/settings.json")).expect("settings");
        assert!(settings.contains(source.to_string_lossy().as_ref()));

        let disabled = set_lockfile_plugin_enabled("omp", &omp_plugins[0].root_path, false)
            .expect("disable omp");
        assert_eq!(disabled.enabled_state, "disabled");
        let disabled_lock = fs::read_to_string(home.join(".omp/plugins/omp-plugins.lock.json"))
            .expect("read disabled lock");
        assert!(disabled_lock.contains("\"enabled\": false"));
        let omp_settings =
            fs::read_to_string(home.join(".omp/agent/settings.json")).expect("omp settings");
        assert!(!omp_settings.contains("plugins/demo"));
        let pi_settings =
            fs::read_to_string(home.join(".pi/agent/settings.json")).expect("pi settings");
        assert!(pi_settings.contains(source.to_string_lossy().as_ref()));
        assert_eq!(scan_lockfile_plugins("omp").len(), 1);

        let enabled = set_lockfile_plugin_enabled("omp", &omp_plugins[0].root_path, true)
            .expect("enable omp");
        assert_eq!(enabled.enabled_state, "enabled");
        let omp_settings =
            fs::read_to_string(home.join(".omp/agent/settings.json")).expect("omp settings");
        assert!(omp_settings.contains("plugins/demo"));

        let disabled_pi = set_lockfile_plugin_enabled("pi", &pi_link.to_string_lossy(), false)
            .expect("disable pi");
        assert_eq!(disabled_pi.enabled_state, "disabled");
        let pi_settings =
            fs::read_to_string(home.join(".pi/agent/settings.json")).expect("pi settings");
        assert!(!pi_settings.contains("plugins/demo"));
        assert_eq!(scan_lockfile_plugins("pi").len(), 1);
        let enabled_pi =
            set_lockfile_plugin_enabled("pi", &pi_link.to_string_lossy(), true).expect("enable pi");
        assert_eq!(enabled_pi.enabled_state, "enabled");
        let pi_settings =
            fs::read_to_string(home.join(".pi/agent/settings.json")).expect("pi settings");
        assert!(pi_settings.contains("plugins/demo"));

        remove_lockfile_install("omp", &omp_plugins[0].root_path).expect("remove omp");
        assert!(!omp_link.exists());
        assert!(scan_lockfile_plugins("omp").is_empty());
        let pi_plugins = scan_lockfile_plugins("pi");
        assert_eq!(pi_plugins.len(), 1);

        fs::write(
            home.join(".pi/agent/settings.json"),
            format!(
                r#"{{
  "packages": [
    "{}",
    "@weiping/unrelated",
    "demo-pi",
    "npm:demo-pi@0.4.0",
    {{ "source": "{}" }},
    {{ "source": "npm:@weiping/unrelated" }}
  ]
}}
"#,
                source.display(),
                source.display(),
            ),
        )
        .expect("write mixed Pi package references");
        remove_lockfile_install("pi", &pi_plugins[0].root_path).expect("remove pi");
        assert!(!pi_link.exists());
        assert!(scan_lockfile_plugins("pi").is_empty());
        let settings =
            fs::read_to_string(home.join(".pi/agent/settings.json")).expect("read settings");
        assert!(!settings.contains(source.to_string_lossy().as_ref()));
        assert!(!settings.contains("\"demo-pi\""));
        assert!(!settings.contains("npm:demo-pi@0.4.0"));
        assert!(settings.contains("@weiping/unrelated"));

        match previous_home {
            Some(value) => env::set_var("HOME", value),
            None => env::remove_var("HOME"),
        }
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn expands_pi_skills_directory_into_individual_skills() {
        let root = temp_home("pi-skills");
        fs::create_dir_all(root.join("skills/neon-postgres")).expect("skill dir");
        fs::create_dir_all(root.join("skills/neon-auth")).expect("skill dir");
        fs::write(
            root.join("skills/neon-postgres/SKILL.md"),
            "---\ndescription: Query Neon Postgres\n---\n# Neon\n",
        )
        .expect("skill file");
        fs::write(
            root.join("skills/neon-auth/SKILL.md"),
            "---\ndescription: Configure Neon Auth\n---\n# Auth\n",
        )
        .expect("skill file");
        fs::write(
            root.join("package.json"),
            r#"{"name":"@neon/skills","pi":{"skills":["./skills"]}}"#,
        )
        .expect("package");

        let mut components = Vec::new();
        append_extra_components(&root, "pi:neon", &mut components);
        let skills = components
            .iter()
            .filter(|component| component.asset_type == "skill")
            .map(|component| {
                (
                    component.name.as_str(),
                    component.id.as_str(),
                    component.description.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            skills,
            vec![
                ("neon-auth", "skills/neon-auth", "Configure Neon Auth"),
                (
                    "neon-postgres",
                    "skills/neon-postgres",
                    "Query Neon Postgres"
                ),
            ]
        );
        assert!(components
            .iter()
            .all(|component| component.package_item_id != "skills"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scans_pi_skills_mcp_and_hooks_as_separate_components() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = temp_home("pi-components");
        let previous_home = env::var_os("HOME");
        env::set_var("HOME", &home);
        fs::create_dir_all(home.join(".pi")).expect("pi home");
        let source = home.join(".skilldock/plugins/demo");
        fs::create_dir_all(source.join("skills/alpha")).expect("skill dir");
        fs::create_dir_all(source.join("extensions")).expect("extension dir");
        fs::create_dir_all(source.join("hooks")).expect("hook dir");
        fs::write(
            source.join("skills/alpha/SKILL.md"),
            "---\ndescription: Alpha skill\n---\n# Alpha\n",
        )
        .expect("skill file");
        fs::write(source.join("extensions/index.ts"), "export {}\n").expect("extension");
        fs::write(source.join("hooks/post.sh"), "echo ok\n").expect("hook");
        fs::write(
            source.join("mcp.json"),
            r#"{"mcpServers":{"neon":{"url":"https://mcp.neon.tech/mcp"}}}"#,
        )
        .expect("mcp");
        fs::write(
            source.join("package.json"),
            r#"{"name":"demo-pi","version":"1.0.0","description":"Demo","pi":{"skills":["./skills"],"extensions":["./extensions"]}}"#,
        )
        .expect("package");

        install_lockfile_plugin(&home, &source, "pi").expect("install pi");
        let plugins = scan_lockfile_plugins("pi");
        assert_eq!(plugins.len(), 1);
        let components = &plugins[0].components;
        assert!(components.iter().any(|component| {
            component.asset_type == "skill"
                && component.name == "alpha"
                && component.id == "skills/alpha"
                && component.description == "Alpha skill"
        }));
        assert!(components.iter().any(|component| {
            component.asset_type == "mcp"
                && component.name == "neon"
                && component.id == "mcp.json/neon"
        }));
        assert!(components.iter().any(|component| {
            component.asset_type == "hook" && component.id == "extensions/index.ts"
        }));
        assert!(components
            .iter()
            .any(|component| component.asset_type == "hook" && component.id == "hooks/post.sh"));
        assert!(components
            .iter()
            .all(|component| component.package_item_id != "skills"));

        match previous_home {
            Some(value) => env::set_var("HOME", value),
            None => env::remove_var("HOME"),
        }
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn scans_pi_plugin_with_recorded_git_source() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = temp_home("pi-source");
        let previous_home = env::var_os("HOME");
        env::set_var("HOME", &home);
        fs::create_dir_all(home.join(".pi")).expect("pi home");
        let source = home.join(".skilldock/plugins/compound-engineering-plugin");
        fs::create_dir_all(source.join(".skilldock")).expect("metadata dir");
        fs::write(
            source.join("package.json"),
            r#"{"name":"compound-engineering","version":"3.30.3","description":"Official Compound Engineering skills plugin for coding agents","pi":{"skills":["./skills"]}}"#,
        )
        .expect("package");
        fs::write(
            source.join("plugin.json"),
            r#"{"name":"compound-engineering","description":"Brainstorm, plan, debug, review, and compound learnings with AI agents","version":"3.30.3"}"#,
        )
        .expect("plugin manifest");
        fs::write(
            source.join(".skilldock/plugin-source.json"),
            r#"{"sourceUrl":"https://github.com/everyinc/compound-engineering-plugin","sourceType":"git","sourceRef":"main","sourceRevision":"abc123"}"#,
        )
        .expect("source metadata");

        install_lockfile_plugin(&home, &source, "pi").expect("install pi");
        let plugins = scan_lockfile_plugins("pi");
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest_name, "compound-engineering");
        assert_eq!(
            plugins[0].description,
            "Brainstorm, plan, debug, review, and compound learnings with AI agents"
        );
        assert_eq!(
            plugins[0].source_url,
            "https://github.com/everyinc/compound-engineering-plugin"
        );
        assert_eq!(plugins[0].source_type, "git");
        assert_eq!(plugins[0].source_ref, "main");
        assert_eq!(plugins[0].source_revision, "abc123");
        assert!(plugins[0].is_git_repo);
        assert_ne!(plugins[0].local_updated_at, plugins[0].last_scanned_at);

        match previous_home {
            Some(value) => env::set_var("HOME", value),
            None => env::remove_var("HOME"),
        }
        let _ = fs::remove_dir_all(home);
    }
}
