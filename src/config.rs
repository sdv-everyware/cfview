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

//! Settings: auto-detection, with everything overridable.
//!
//! The idea for a standalone tool: work out of the box on someone else's
//! machine. Whatever the system can tell us, we find out ourselves
//! (ring::find_view takes the VMs' adb addresses from cuttlefish_config.json,
//! and the adb path is searched here). Everything else can be overridden, in
//! order of priority:
//!
//!     env variables  >  cfview.json  >  auto-detection / defaults
//!
//! The config file is optional and is looked up as ./cfview.json, then
//! $XDG_CONFIG_HOME/cfview.json (~/.config/cfview.json):
//!
//!     {
//!       "adb": "/path/to/adb",
//!       "serials": { "0": "0.0.0.0:6520", "1": "0.0.0.0:6521" }
//!     }

use std::collections::HashMap;
use std::path::PathBuf;

pub struct Settings {
    /// Path to the adb binary.
    pub adb: String,
    /// Touch routing boundary (width of the overlay strip), px. No longer used:
    /// touch is routed by overlay alpha (see main.rs).
    pub har_width: i32,
    /// Overrides of the adb serials by VM index (usually not needed: the
    /// addresses come from cuttlefish_config.json automatically).
    pub serials: HashMap<u32, String>,
}

pub fn load() -> Settings {
    let file = load_file();
    let file_get = |key: &str| file.as_ref().and_then(|v| v.get(key).and_then(|x| x.as_str().map(String::from)));

    let mut serials: HashMap<u32, String> = HashMap::new();
    if let Some(map) = file.as_ref().and_then(|v| v.get("serials")).and_then(|s| s.as_obj()) {
        for (k, v) in map {
            if let (Ok(idx), Some(s)) = (k.parse(), v.as_str()) {
                serials.insert(idx, s.to_string());
            }
        }
    }
    // env overrides of the serials: VM0_SERIAL, VM1_SERIAL, ... VM9_SERIAL.
    for idx in 0..10u32 {
        if let Ok(s) = std::env::var(format!("VM{idx}_SERIAL")) {
            serials.insert(idx, s);
        }
    }

    let har_width = std::env::var("HAR_WIDTH")
        .ok()
        .or_else(|| {
            file.as_ref()
                .and_then(|v| v.get("har_width"))
                .and_then(|n| n.as_u64())
                .map(|n| n.to_string())
        })
        .and_then(|s| s.parse().ok())
        .unwrap_or(960);

    let adb = std::env::var("ADB").ok().or_else(|| file_get("adb")).unwrap_or_else(find_adb);

    Settings { adb, har_width, serials }
}

fn load_file() -> Option<crate::json::Value> {
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from("cfview.json")];
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        candidates.push(PathBuf::from(xdg).join("cfview.json"));
    } else if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(home).join(".config/cfview.json"));
    }
    for path in candidates {
        if let Ok(text) = std::fs::read_to_string(&path) {
            match crate::json::parse(&text) {
                Ok(v) => {
                    println!("cfview: config file {}", path.display());
                    return Some(v);
                }
                Err(e) => eprintln!("cfview: ignoring broken {}: {e}", path.display()),
            }
        }
    }
    None
}

/// Looks for adb where it usually lives. The order reflects specificity: the
/// user's PATH is their choice, then the standard places of the Android SDK and
/// of the system cuttlefish package.
fn find_adb() -> String {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let cand = PathBuf::from(dir).join("adb");
            if is_executable(&cand) {
                return cand.to_string_lossy().into_owned();
            }
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(&home).join("Android/Sdk/platform-tools/adb"));
    }
    candidates.push(PathBuf::from("/usr/lib/cuttlefish-common/bin/adb"));
    candidates.push(PathBuf::from("/opt/android-sdk/platform-tools/adb"));
    for cand in candidates {
        if is_executable(&cand) {
            return cand.to_string_lossy().into_owned();
        }
    }
    // Last resort: let execvp itself report "not found" with a clear error
    // on first use.
    "adb".to_string()
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
