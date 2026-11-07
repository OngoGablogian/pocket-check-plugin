//! Ignore list (Pocket-Check-Ignore.txt) - same rules as the Python script.

use crate::fsx::glob;
use std::cell::RefCell;
use std::collections::HashSet;

pub const IGNORE_NAME: &str = "Pocket-Check-Ignore.txt";

pub const DEFAULT_IGNORE_TEXT: &str = r##"# Pocket-Check-Ignore.txt - things that Pocket-Check should ignore.
# (Created automatically because this file was missing.)
#
# One entry per line. Lines starting with # (and anything after " #") are comments.
# Matching is case-insensitive and * and ? wildcards are allowed.
#
# 1) CORES - a core folder name (the folder inside /Cores), e.g. console cores that
#    don't use instance .json files. The core is skipped everywhere: /Cores, its
#    /Assets/<platform>/<core> folder, its /Presets folder, and any platform that
#    only ignored cores use.
#      Mazamars312.PC Engine CD
#
# 2) PATHS - any file or folder exactly as shown in the report, relative to the SD
#    card root (use /). Problems about that path are hidden; a folder covers
#    everything inside it.
#      Assets/ng/common/uni-bios_4_0.rom
#      Assets/ng/common/xenocrisis
#      Assets/*/common/*.rom
#
# 3) FILE NAMES - a bare file name (no /) hides problems about that file wherever
#    it is on the card.
#      uni-bios_4_0.rom
#
# Everything ignored is listed in the report under "== Ignored ... ==" sections
# (one for cores, one for folders and one for files).
#
# Examples (remove the leading # to enable):
# Mazamars312.PC Engine CD
# Assets/ng/common/uni-bios_4_0.rom
"##;

/// Entries as written (comments / quotes removed).
pub fn parse_ignore_list(text: &str) -> Vec<String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut cut = line.len();
        let mut prev_ws = false;
        for (i, c) in line.char_indices() {
            if c == '#' && prev_ws {
                cut = i;
                break;
            }
            prev_ws = c.is_whitespace();
        }
        let l = line[..cut].trim().trim_matches(|c: char| c == '"' || c == '\'');
        if !l.is_empty() {
            out.push(l.to_string());
        }
    }
    out
}

/// An entry with no `/` is a core folder name AND a file name to hide anywhere;
/// an entry with a `/` is a path relative to the SD root.
pub struct IgnoreList {
    entries: Vec<String>,
    cores: Vec<(String, String)>,
    names: Vec<(String, String)>,
    paths: Vec<(String, String)>,
    used: RefCell<HashSet<String>>,
}

impl IgnoreList {
    pub fn new(entries: Vec<String>) -> Self {
        let mut cores = Vec::new();
        let mut names = Vec::new();
        let mut paths = Vec::new();
        for raw in &entries {
            let mut norm = raw.replace('\\', "/").trim().to_string();
            while norm.starts_with("./") {
                norm = norm[2..].to_string();
            }
            let norm = norm.trim_matches('/').to_lowercase();
            if norm.is_empty() {
                continue;
            }
            if norm.contains('/') {
                paths.push((raw.clone(), norm.clone()));
                // Cores/<name> also ignores the core
                if norm.starts_with("cores/") && norm.matches('/').count() == 1 {
                    if let Some(n) = norm.split('/').nth(1) {
                        cores.push((raw.clone(), n.to_string()));
                    }
                }
            } else {
                cores.push((raw.clone(), norm.clone()));
                names.push((raw.clone(), norm));
            }
        }
        Self { entries, cores, names, paths, used: RefCell::new(HashSet::new()) }
    }

    pub fn core(&self, name: &str) -> bool {
        let n = name.to_lowercase();
        let mut hit = false;
        for (raw, pat) in &self.cores {
            if n == *pat || glob(pat, &n) {
                self.used.borrow_mut().insert(raw.clone());
                hit = true;
            }
        }
        hit
    }

    pub fn path(&self, p: &str) -> bool {
        let low = p.to_lowercase();
        for (raw, pat) in &self.paths {
            if low == *pat
                || glob(pat, &low)
                || low.starts_with(&format!("{pat}/"))
                || glob(&format!("{pat}/*"), &low)
            {
                self.used.borrow_mut().insert(raw.clone());
                return true;
            }
        }
        let base = low.rsplit('/').next().unwrap_or("");
        for (raw, pat) in &self.names {
            if glob(pat, base) {
                self.used.borrow_mut().insert(raw.clone());
                return true;
            }
        }
        false
    }

    pub fn unmatched(&self) -> Vec<String> {
        let used = self.used.borrow();
        self.entries.iter().filter(|e| !used.contains(*e)).cloned().collect()
    }
}
