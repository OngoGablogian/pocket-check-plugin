//! Pocket-Check - openFPGA Pocket updater plugin.
//!
//! Port of Pocket-Check.py: scans the Pocket SD card (exposed to plugins as the
//! `pocket/` folder) for missing / empty / orphaned / mismatched ROMs and instance
//! .json files, and writes a report. It never changes anything on the card.
//!
//! Files the user can read and edit live on the SD card, in the plugin's own folder
//! (`/plugins/OngoGablogian.pocket-check-plugin/` - the folder pupdate puts the plugin in),
//! falling back to the root of the SD card. Nothing is written to the computer.
//!   Pocket-Check-Ignore.txt     ignore list (created on first run)
//!   Pocket-Check-Manifest.json  optional: { "sf2.rom": { "size": 4194304 } }
//!   Pocket-Check-Report.txt     the report (rewritten every run)

mod check;
mod fsx;
mod ignore;

use extism_pdk::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

const VERSION: &str = "0.2.1";
const REPORT_NAME: &str = "Pocket-Check-Report.txt";
const MANIFEST_NAME: &str = "Pocket-Check-Manifest.json";
/// Reports longer than this many lines are summarised in the log (full text is in the file)
const MAX_LOG_LINES: usize = 120;

const M_WHOLE: &str = "Scan the whole card";
const M_ONE: &str = "Scan one platform...";
const M_EXIT: &str = "Exit";
const O_DEFAULT: &str = "Default (skip .sav and Analogizer files)";
const O_SAV: &str = "Also check .sav files";
const O_ANALOGIZER: &str = "Also check Analogizer/CRT files";
const O_BOTH: &str = "Check .sav and Analogizer files";

#[derive(Clone, Serialize, ToBytes)]
#[encoding(Json)]
enum PluginMessage {
    Choice {
        name: String,
        query: String,
        choices: Vec<String>,
    },
    Text {
        name: String,
        query: String,
    },
    Exit,
}

#[derive(Clone, Deserialize, FromBytes)]
#[encoding(Json)]
enum HostMessage {
    Answer { name: String, value: String },
    Kill,
}

#[host_fn]
unsafe extern "ExtismHost" {
    fn print_msg(message: &str) -> ();
}

fn print(msg: &str) {
    unsafe {
        let _ = print_msg(msg);
    };
}

/// Print a line to the host's log.
pub fn say(msg: &str) {
    print(&format!("{msg}\n"));
}

#[plugin_fn]
pub fn start() -> FnResult<PluginMessage> {
    say(&format!("Pocket-Check v{VERSION}"));
    if !fsx::is_dir("Assets") {
        say("Can't see an Assets folder on the Pocket SD card - nothing to check.");
        return Ok(PluginMessage::Exit);
    }
    Ok(PluginMessage::Choice {
        name: "main".to_string(),
        query: format!("Pocket-Check v{VERSION} - what do you want to scan?"),
        choices: vec![M_WHOLE.to_string(), M_ONE.to_string(), M_EXIT.to_string()],
    })
}

fn options_menu(filter: &str) -> PluginMessage {
    PluginMessage::Choice {
        // the platform filter travels in the question name, so no state is kept between calls
        name: format!("opts:{filter}"),
        query: format!("Pocket-Check v{VERSION} - which files should be checked?"),
        choices: vec![
            O_DEFAULT.to_string(),
            O_SAV.to_string(),
            O_ANALOGIZER.to_string(),
            O_BOTH.to_string(),
        ],
    }
}

#[plugin_fn]
pub fn handle_response(input: HostMessage) -> FnResult<PluginMessage> {
    match input {
        HostMessage::Kill => Ok(PluginMessage::Exit),
        HostMessage::Answer { name, value } => {
            if name == "main" {
                return Ok(match value.as_str() {
                    M_WHOLE => options_menu(""),
                    M_ONE => PluginMessage::Text {
                        name: "platform".to_string(),
                        query: "Platform id (globs ok, comma separated - e.g. jtbubl or jt*, arcade)?".to_string(),
                    },
                    _ => {
                        say("Exiting...");
                        PluginMessage::Exit
                    }
                });
            }
            if name == "platform" {
                let filter = value.trim();
                if filter.is_empty() {
                    say("No platform given, scanning the whole card.");
                }
                return Ok(options_menu(filter));
            }
            if let Some(filter) = name.strip_prefix("opts:") {
                let (sav, analogizer) = match value.as_str() {
                    O_SAV => (true, false),
                    O_ANALOGIZER => (false, true),
                    O_BOTH => (true, true),
                    _ => (false, false),
                };
                let platforms: Vec<String> = filter
                    .split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect();
                run_scan(check::Opts { include_sav: sav, include_analogizer: analogizer, platforms });
            }
            Ok(PluginMessage::Exit)
        }
    }
}

/// The files you read and edit live on the SD card, in the plugin's own folder - the folder
/// pupdate installs the plugin into (named <github owner>.<repo>). If you publish the plugin
/// under a different GitHub owner or repo name, change this to match.
const PLUGIN_DIR: &str = "pocket/plugins/OngoGablogian.pocket-check-plugin";
/// Fallback if the plugin's folder can't be written to.
const SD_ROOT: &str = "pocket";
/// The host app's private folder on the computer. Only READ, to pick up files that older
/// versions (v0.1.0, v0.1.1, v0.2.0) left there; nothing is ever written to it.
const HOST_DIR: &str = "host";

/// Human-readable location for the log.
fn show(path: &str) -> String {
    if let Some(name) = path.strip_prefix(&format!("{PLUGIN_DIR}/")) {
        format!("the plugin's folder on the SD card ({name})")
    } else if let Some(name) = path.strip_prefix("host/") {
        format!("the updater's private plugin folder on the computer ({name})")
    } else if let Some(name) = path.strip_prefix("pocket/") {
        format!("the SD card root ({name})")
    } else {
        path.to_string()
    }
}

fn exists(path: &str) -> bool {
    fs::File::open(path).is_ok()
}

/// Make sure /plugins/<this plugin>/ exists on the card (it normally does). Uses create_dir,
/// which only fails harmlessly if the folder is already there.
fn ensure_plugin_dir() {
    let _ = fs::create_dir("pocket/plugins");
    let _ = fs::create_dir(PLUGIN_DIR);
}

/// Write a file to the plugin's folder on the SD card, or to the SD card root if that fails.
/// `atomic` writes to a .tmp file first and renames it, so a stopped run never leaves half a file.
fn save(name: &str, data: &[u8], atomic: bool) -> Result<String, String> {
    let mut errors: Vec<String> = Vec::new();
    ensure_plugin_dir();
    for dir in [PLUGIN_DIR, SD_ROOT] {
        let path = format!("{dir}/{name}");
        let res = if atomic {
            let tmp = format!("{path}.tmp");
            fs::write(&tmp, data).and_then(|_| fs::rename(&tmp, &path))
        } else {
            fs::write(&path, data)
        };
        match res {
            Ok(()) => return Ok(path),
            Err(e) => errors.push(format!("{path}: {e}")),
        }
    }
    Err(errors.join("; "))
}

fn run_scan(opts: check::Opts) {
    // ignore list - created with instructions on the first run
    let plugin_ignore = format!("{PLUGIN_DIR}/{}", ignore::IGNORE_NAME);
    let older_ignore = [SD_ROOT, HOST_DIR]
        .iter()
        .map(|d| format!("{d}/{}", ignore::IGNORE_NAME))
        .find(|p| exists(p));
    let mut created_note: Option<String> = None;
    let ignore_path: Option<String> = if exists(&plugin_ignore) {
        Some(plugin_ignore)
    } else if let Some(old) = older_ignore {
        // left by an older version in the SD card root or the host folder: copy it into the
        // plugin's folder on the SD card
        ensure_plugin_dir();
        match fsx::read_path(&old).and_then(|b| fs::write(&plugin_ignore, &b)) {
            Ok(()) => {
                let n = format!(
                    "Copied your ignore list from {} to the plugin's folder on the SD card; the old copy is no longer used and can be deleted",
                    show(&old)
                );
                say(&n);
                created_note = Some(n);
                Some(plugin_ignore)
            }
            Err(_) => Some(old),
        }
    } else {
        match save(ignore::IGNORE_NAME, ignore::DEFAULT_IGNORE_TEXT.as_bytes(), false) {
            Ok(p) => {
                let n = format!("Created default ignore list in {} - see the instructions inside it", show(&p));
                say(&n);
                created_note = Some(n);
                Some(p)
            }
            Err(e) => {
                say(&format!("Could not create {}: {e}", ignore::IGNORE_NAME));
                None
            }
        }
    };
    let (ignore_entries, ignore_src) = match ignore_path.and_then(|p| fsx::read_path(&p).ok().map(|b| (b, p))) {
        Some((bytes, p)) => (ignore::parse_ignore_list(&String::from_utf8_lossy(&bytes)), Some(p)),
        None => (Vec::new(), None),
    };

    // optional manifest: expected size per ROM filename
    let mut manifest: HashMap<String, u64> = HashMap::new();
    let manifest_path = [PLUGIN_DIR, SD_ROOT, HOST_DIR]
        .iter()
        .map(|d| format!("{d}/{MANIFEST_NAME}"))
        .find(|p| exists(p));
    if let Some(manifest_path) = manifest_path {
        match fsx::read_path(&manifest_path).map_err(|e| e.to_string()).and_then(|b| fsx::parse_json(&b)) {
            Ok(v) => {
                if let Some(obj) = v.as_object() {
                    for (k, e) in obj {
                        if let Some(size) = e.get("size").and_then(|s| s.as_u64()) {
                            manifest.insert(k.to_lowercase(), size);
                        }
                    }
                    say(&format!("Loaded {manifest_path} ({} sizes)", manifest.len()));
                }
            }
            Err(e) => say(&format!("Can't read {manifest_path}: {e}")),
        }
    }

    let inputs = check::Inputs { ignore_entries, ignore_src, created_note, manifest };
    let out = match check::run(&opts, inputs) {
        Ok(o) => o,
        Err(e) => {
            say(&e);
            return;
        }
    };

    let header = format!("Pocket-Check v{VERSION} (plugin) - {}\n\n", timestamp());
    let full = format!("{header}{}\n", out.text);
    let saved = save(REPORT_NAME, full.as_bytes(), true);

    say("");
    if out.text.lines().count() <= MAX_LOG_LINES || saved.is_err() {
        say(&out.text);
    } else {
        say("Scan finished.");
        for (cat, n) in &out.categories {
            say(&format!("  {cat}: {n}"));
        }
        say(&if out.problems == 0 {
            "No problems found.".to_string()
        } else {
            format!("{} problem(s) found.", out.problems)
        });
    }
    match &saved {
        Ok(p) => say(&format!("\nReport written to {}", show(p))),
        Err(e) => say(&format!("\nCould not write the report file ({REPORT_NAME}): {e}")),
    }
}

fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if secs == 0 {
        return "unknown date".to_string();
    }
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}
