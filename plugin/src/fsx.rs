//! File-system and small string/JSON helpers.
//!
//! All card paths are handled as "relative to the SD root" posix strings
//! ("Assets/jtbubl/common/x.rom"). `abs()` maps them into the plugin sandbox,
//! where the Pocket's SD card is exposed as the `pocket/` folder.

use serde_json::Value;
use std::collections::HashMap;
use std::fs;

pub const ROOT: &str = "pocket";

pub fn abs(rel: &str) -> String {
    if rel.is_empty() {
        ROOT.to_string()
    } else {
        format!("{ROOT}/{rel}")
    }
}

pub fn join(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else if b.is_empty() {
        a.to_string()
    } else {
        format!("{a}/{b}")
    }
}

pub fn dirname(p: &str) -> String {
    match p.rfind('/') {
        Some(i) => p[..i].to_string(),
        None => String::new(),
    }
}

pub fn basename(p: &str) -> &str {
    match p.rfind('/') {
        Some(i) => &p[i + 1..],
        None => p,
    }
}

/// Like Python's posixpath.normpath for relative paths ("a/./b/../c" -> "a/c").
pub fn normpath(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if matches!(out.last(), Some(l) if *l != "..") {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            _ => out.push(part),
        }
    }
    out.join("/")
}

pub fn is_dir(rel: &str) -> bool {
    fs::metadata(abs(rel)).map(|m| m.is_dir()).unwrap_or(false)
}

pub fn is_file(rel: &str) -> bool {
    fs::metadata(abs(rel)).map(|m| m.is_file()).unwrap_or(false)
}

pub fn file_size(rel: &str) -> u64 {
    fs::metadata(abs(rel)).map(|m| m.len()).unwrap_or(0)
}

pub fn parse_json(bytes: &[u8]) -> Result<Value, String> {
    let bom = [0xEFu8, 0xBB, 0xBF];
    let body = bytes.strip_prefix(&bom[..]).unwrap_or(bytes);
    serde_json::from_slice(body).map_err(|e| e.to_string())
}

pub fn read_json(rel: &str) -> Result<Value, String> {
    let bytes = fs::read(abs(rel)).map_err(|e| e.to_string())?;
    parse_json(&bytes)
}

/// Shell-style match with `*` and `?` (callers lowercase both sides).
pub fn glob(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Directory listing cache plus the few helpers that need it.
#[derive(Default)]
pub struct Fs {
    ls: HashMap<String, Vec<String>>,
}

impl Fs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sorted names in a folder, without macOS "._*" AppleDouble entries.
    pub fn listdir(&mut self, rel: &str) -> Vec<String> {
        if let Some(v) = self.ls.get(rel) {
            return v.clone();
        }
        let mut names: Vec<String> = match fs::read_dir(abs(rel)) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.starts_with("._"))
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        self.ls.insert(rel.to_string(), names.clone());
        names
    }

    /// Resolve `rel` under `root`. Returns (path as it is on the card, letter case differs).
    pub fn find_exact(&mut self, root: &str, rel: &str) -> (Option<String>, bool) {
        let mut cur = root.to_string();
        let mut case = false;
        for part in rel.split('/').filter(|p| !p.is_empty()) {
            let names = self.listdir(&cur);
            if names.iter().any(|n| n == part) {
                cur = join(&cur, part);
                continue;
            }
            let lower = part.to_lowercase();
            match names.iter().find(|n| n.to_lowercase() == lower) {
                None => return (None, false),
                Some(alt) => {
                    cur = join(&cur, alt);
                    case = true;
                }
            }
        }
        if is_file(&cur) { (Some(cur), case) } else { (None, case) }
    }

    /// Top-down walk like os.walk. Returns (folder, files in it). Skips "._*" entries and,
    /// directly under `top`, sub-folders for which `skip_top(name)` is true.
    pub fn walk(&mut self, top: &str, skip_top: &dyn Fn(&str) -> bool) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        let mut stack = vec![top.to_string()];
        while let Some(dir) = stack.pop() {
            let mut files = Vec::new();
            let mut dirs = Vec::new();
            for n in self.listdir(&dir) {
                let p = join(&dir, &n);
                if is_dir(&p) {
                    if dir == top && skip_top(&n) {
                        continue;
                    }
                    dirs.push(p);
                } else {
                    files.push(n);
                }
            }
            out.push((dir, files));
            for d in dirs.into_iter().rev() {
                stack.push(d);
            }
        }
        out
    }
}
