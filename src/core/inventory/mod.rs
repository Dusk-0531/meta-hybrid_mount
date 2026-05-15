// Copyright (C) 2026 YuzakiKokuban <heibanbaize@gmail.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

pub mod discovery;
pub mod listing;

pub use discovery::*;

#[cfg(not(feature = "control-plane"))]
use crate::domain::MountMode;
use crate::{conf::config::Config, defs, domain::ModuleRules};

pub fn load_module_rules(config: &Config, module_id: &str) -> ModuleRules {
    let mut rules = ModuleRules {
        default_mode: config.default_mode.as_mount_mode(),
        ..Default::default()
    };

    if let Some(global_rules) = config.rules.get(module_id) {
        rules.default_mode = global_rules.default_mode;
        rules.paths.extend(global_rules.paths.clone());
    }

    #[cfg(not(feature = "control-plane"))]
    apply_nano_rules(config, &mut rules);

    rules
}

#[cfg(not(feature = "control-plane"))]
fn apply_nano_rules(config: &Config, rules: &mut ModuleRules) {
    let overlay_whitelist = config
        .overlay_whitelist
        .iter()
        .map(|path| normalize_rule_path(path))
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();

    let mut normalized_paths = std::collections::HashMap::new();
    for (path, mode) in rules.paths.drain() {
        let path = normalize_rule_path(std::path::Path::new(&path));
        if path.is_empty() {
            continue;
        }
        let mode = match mode {
            MountMode::Ignore => MountMode::Ignore,
            _ if overlay_whitelist
                .iter()
                .any(|prefix| path_matches_policy_prefix(&path, prefix)) =>
            {
                MountMode::Overlay
            }
            _ => MountMode::Magic,
        };
        normalized_paths.insert(path, mode);
    }

    for path in overlay_whitelist {
        normalized_paths.insert(path, MountMode::Overlay);
    }

    rules.default_mode = MountMode::Magic;
    rules.paths = normalized_paths;
}

#[cfg(not(feature = "control-plane"))]
fn normalize_rule_path(path: &std::path::Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(not(feature = "control-plane"))]
fn path_matches_policy_prefix(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub fn is_reserved_module_dir(id: &str) -> bool {
    matches!(
        id,
        "hybrid-mount" | "hybrid_mount" | "lost+found" | ".git" | ".idea" | ".vscode"
    )
}

pub fn mount_block_markers(module_path: &std::path::Path) -> Vec<&'static str> {
    let mut markers = Vec::new();
    if module_path.join(defs::DISABLE_FILE_NAME).exists() {
        markers.push(defs::DISABLE_FILE_NAME);
    }
    if module_path.join(defs::REMOVE_FILE_NAME).exists() {
        markers.push(defs::REMOVE_FILE_NAME);
    }
    if module_path.join(defs::MOUNT_ERROR_FILE_NAME).exists() {
        markers.push(defs::MOUNT_ERROR_FILE_NAME);
    }
    if module_path.join(defs::SKIP_HYBRID_FILE_NAME).exists() {
        markers.push(defs::SKIP_HYBRID_FILE_NAME);
    }
    markers
}

pub fn has_mount_block_marker(module_path: &std::path::Path) -> bool {
    !mount_block_markers(module_path).is_empty()
}
