//! The checks - a port of Pocket-Check.py v0.6.1.
//!
//! Scans Cores, Assets, Platforms and Presets on the SD card (the `pocket/` folder
//! inside the plugin sandbox) and cross-checks them.

use crate::fsx::*;
use crate::ignore::IgnoreList;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

const CORE_FILES: [&str; 7] = [
    "core.json", "data.json", "video.json", "input.json", "interact.json", "audio.json", "variants.json",
];
const ANALOGIZER_FILES: [&str; 3] = ["crtcfg.bin", "analogizer.bin", "analogizer.json"];
const ANALOGIZER_PLATFORMS: [&str; 1] = ["analogizer"];
const ALWAYS_SKIP_FILES: [&str; 4] = ["jtpatreon.bin", "jtpatreon.json", "coinopkey.bin", "coinopkey.json"];
const ALWAYS_SKIP_PLATFORMS: [&str; 2] = ["jtpatreon", "coinopkey"];
const ROM_EXTS: [&str; 1] = [".rom"];

/// Builds a `&[&str]` from a mix of String / &str expressions.
macro_rules! ps {
    ($($e:expr),+ $(,)?) => {
        &[$(AsRef::<str>::as_ref(&$e)),+][..]
    };
}

#[derive(Clone, Default)]
pub struct Opts {
    pub include_sav: bool,
    pub include_analogizer: bool,
    /// platform id globs; empty = everything
    pub platforms: Vec<String>,
}

pub struct Inputs {
    pub ignore_entries: Vec<String>,
    pub ignore_src: Option<String>,
    pub created_note: Option<String>,
    /// expected size per ROM filename (lowercase), from the optional manifest
    pub manifest: HashMap<String, u64>,
}

pub struct Output {
    pub text: String,
    pub problems: usize,
    /// (category, count) for every problem category
    pub categories: Vec<(String, usize)>,
}

struct CoreInfo {
    platform_ids: Vec<String>,
    relevant: bool,
    ignored: bool,
}

#[derive(Default)]
struct Cats(Vec<(String, Vec<String>)>);

impl Cats {
    fn push(&mut self, cat: &str, msg: String) {
        match self.0.iter().position(|(c, _)| c.as_str() == cat) {
            Some(i) => self.0[i].1.push(msg),
            None => self.0.push((cat.to_string(), vec![msg])),
        }
    }
    fn total(&self) -> usize {
        self.0.iter().map(|(_, v)| v.len()).sum()
    }
}

// ---------------------------------------------------------------- helpers
fn has(files: &[String], name: &str) -> bool {
    files.iter().any(|f| f.as_str() == name)
}

fn repr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::String(s)) => format!("'{s}'"),
        Some(o) => o.to_string(),
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim().replace('_', "");
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r.to_string()),
        None => (false, t.trim_start_matches('+').to_string()),
    };
    let low = t.to_lowercase();
    let val = if let Some(h) = low.strip_prefix("0x") {
        i64::from_str_radix(h, 16).ok()
    } else if let Some(b) = low.strip_prefix("0b") {
        i64::from_str_radix(b, 2).ok()
    } else if let Some(o) = low.strip_prefix("0o") {
        i64::from_str_radix(o, 8).ok()
    } else {
        low.parse::<i64>().ok()
    };
    val.map(|v| if neg { -v } else { v })
}

fn to_int(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => parse_int(s),
        Some(Value::Bool(b)) => Some(*b as i64),
        _ => None,
    }
}

fn opt_int(v: Option<i64>) -> String {
    v.map(|i| i.to_string()).unwrap_or_else(|| "None".to_string())
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// ^[^.\s][^.]*\.[^.]+$  (AuthorName.CoreName)
fn core_name_ok(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    parts.len() == 2
        && !parts[0].is_empty()
        && !parts[0].starts_with(char::is_whitespace)
        && !parts[1].is_empty()
}

/// ^[a-z0-9][a-z0-9_]{0,14}$
fn plat_id_ok(pid: &str) -> bool {
    let n = pid.chars().count();
    if n == 0 || n > 15 {
        return false;
    }
    pid.chars().enumerate().all(|(i, c)| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || (i > 0 && c == '_')
    })
}

fn strip_chars(s: &str, n: usize) -> String {
    let len = s.chars().count();
    s.chars().take(len.saturating_sub(n)).collect()
}

fn ends_ci(s: &str, ext: &str) -> bool {
    s.to_lowercase().ends_with(ext)
}

// ------------------------------------------------------------------- scan
struct Scan {
    fs: Fs,
    ign: IgnoreList,
    items: Cats,
    notes: Cats,
    ignored: BTreeMap<String, BTreeSet<String>>,
    skip_files: HashSet<String>,
    skip_plats: HashSet<String>,
    ignore_sav: bool,
    filt: Vec<String>,
    manifest: HashMap<String, u64>,
}

impl Scan {
    fn skip_file(&self, name: &str) -> bool {
        let base = basename(&name.replace('\\', "/")).to_lowercase();
        (self.ignore_sav && name.to_lowercase().ends_with(".sav")) || self.skip_files.contains(&base)
    }

    fn skip_plat(&self, pid: &str) -> bool {
        self.skip_plats.contains(&pid.to_lowercase())
    }

    fn matches(&self, pid: &str) -> bool {
        let low = pid.to_lowercase();
        self.filt.is_empty() || self.filt.iter().any(|p| glob(p, &low))
    }

    /// True (and the path is listed as ignored) if any path matches the ignore list.
    fn is_ignored(&mut self, paths: &[&str]) -> bool {
        for p in paths {
            if !p.is_empty() && self.ign.path(p) {
                let sec = if is_dir(p) { "Ignored folders" } else { "Ignored files" };
                self.ignored.entry(sec.to_string()).or_default().insert(p.to_string());
                return true;
            }
        }
        false
    }

    fn add(&mut self, cat: &str, msg: String, paths: &[&str]) {
        if !self.is_ignored(paths) {
            self.items.push(cat, msg);
        }
    }

    fn note(&mut self, cat: &str, msg: String, paths: &[&str]) {
        if !self.is_ignored(paths) {
            self.notes.push(cat, msg);
        }
    }

    // ---------------------------------------------------------------- CORES
    fn scan_cores(&mut self) -> BTreeMap<String, CoreInfo> {
        let mut cores = BTreeMap::new();
        if !is_dir("Cores") {
            self.add("Core folder problems", "No /Cores folder found".to_string(), &[]);
            return cores;
        }
        for name in self.fs.listdir("Cores") {
            let tag = format!("Cores/{name}");
            if !is_dir(&tag) {
                continue;
            }
            let files = self.fs.listdir(&tag);
            let skip = self.ign.core(&name);

            let mut cj = Value::Null;
            if has(&files, "core.json") {
                match read_json(&format!("{tag}/core.json")) {
                    Ok(v) => cj = v,
                    Err(e) => {
                        if !skip {
                            self.add("Core folder problems", format!("{tag}/core.json: {e}"), ps![format!("{tag}/core.json")]);
                        }
                    }
                }
            }
            let core = cj.get("core").cloned().unwrap_or(Value::Null);
            let meta = core.get("metadata").cloned().unwrap_or(Value::Null);
            let platform_ids: Vec<String> = meta
                .get("platform_ids")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();

            let relevant = !skip
                && (self.filt.is_empty() || platform_ids.iter().any(|p| self.matches(p)));
            cores.insert(
                name.clone(),
                CoreInfo { platform_ids: platform_ids.clone(), relevant, ignored: skip },
            );
            if !relevant {
                continue;
            }

            if !core_name_ok(&name) {
                self.add("Core folder problems", format!("{tag}: not in 'AuthorName.CoreName' format"), ps![tag]);
            }
            for req in CORE_FILES {
                if !has(&files, req) {
                    self.add("Core folder problems", format!("{tag}: missing {req}"), ps![format!("{tag}/{req}")]);
                } else if req != "core.json" {
                    if let Err(e) = read_json(&format!("{tag}/{req}")) {
                        self.add("Core folder problems", format!("{tag}/{req}: {e}"), ps![format!("{tag}/{req}")]);
                    }
                }
            }

            // folder name vs core.json author.shortname
            if let (Some(a), Some(s)) = (str_of(&meta, "author"), str_of(&meta, "shortname")) {
                if !a.is_empty() && !s.is_empty() && format!("{a}.{s}") != name {
                    self.add(
                        "Core folder problems",
                        format!("{tag}: core.json says '{a}.{s}'"),
                        ps![tag, format!("{tag}/core.json")],
                    );
                }
            }

            // bitstreams
            if let Some(list) = core.get("cores").and_then(|v| v.as_array()) {
                for b in list {
                    let fnm = match str_of(b, "filename") {
                        Some(f) if !f.is_empty() => f.to_string(),
                        _ => continue,
                    };
                    let (found, case) = self.fs.find_exact(&tag, &fnm);
                    if found.is_none() {
                        self.add(
                            "Core folder problems",
                            format!("{tag}: bitstream '{fnm}' (core.json) not found"),
                            ps![format!("{tag}/{fnm}")],
                        );
                    } else if case {
                        self.add(
                            "Case mismatches",
                            format!("{tag}: bitstream '{fnm}' differs in letter case on card"),
                            ps![format!("{tag}/{fnm}")],
                        );
                    }
                }
            }

            if has(&files, "data.json") {
                self.check_data_slots(&name, &platform_ids);
            }
        }
        cores
    }

    fn check_data_slots(&mut self, name: &str, pids: &[String]) {
        let tag = format!("Cores/{name}/data.json");
        let dj = match read_json(&tag) {
            Ok(v) if v.is_object() => v,
            _ => return,
        };
        let slots: Vec<Value> = dj
            .get("data")
            .and_then(|d| d.get("data_slots"))
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default();
        if slots.len() > 32 {
            self.add("Core folder problems", format!("{tag}: {} data_slots (max 32)", slots.len()), ps![tag]);
        }
        for s in &slots {
            if !s.is_object() {
                continue;
            }
            let fnm = match str_of(s, "filename") {
                Some(f) if !f.is_empty() => f.to_string(),
                _ => continue,
            };
            if truthy(s.get("nonvolatile")) || self.skip_file(&fnm) {
                continue; // no fixed file, or a save slot
            }
            let params = to_int(s.get("parameters")).unwrap_or(0);
            if params & 0x10 != 0 {
                continue; // instance JSON slot - user picks the file
            }
            let slot_name = str_of(s, "name").unwrap_or("None").to_string();
            if fnm.chars().count() > 31 {
                self.add(
                    "Core folder problems",
                    format!("{tag}: slot '{slot_name}' filename longer than 31 chars"),
                    ps![tag],
                );
            }
            let idx = ((params >> 24) & 0x3) as usize;
            if idx >= pids.len() {
                self.add(
                    "Core folder problems",
                    format!("{tag}: slot '{slot_name}' uses platform index {idx} but core.json lists {}", pids.len()),
                    ps![tag],
                );
                continue;
            }
            if self.skip_plat(&pids[idx]) {
                continue;
            }
            let folder = if params & 0x2 != 0 { name } else { "common" };
            let path = format!("Assets/{}/{}/{}", pids[idx], folder, fnm);
            let (found, case) = self.fs.find_exact("", &path);
            match found {
                None => {
                    let msg = format!("{path}   (needed by {name}, slot '{slot_name}')");
                    if truthy(s.get("required")) {
                        self.add("Missing core data files", msg, ps![path]);
                    } else {
                        self.note("Optional core data files not present", msg, ps![path]);
                    }
                }
                Some(f) => {
                    if case {
                        self.add("Case mismatches", format!("{path} differs in letter case on card"), ps![path]);
                    } else if file_size(&f) == 0 {
                        self.add("Empty ROMs", path.clone(), ps![path]);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ PLATFORMS
    fn check_platforms(&mut self, cores: &BTreeMap<String, CoreInfo>) {
        let used: BTreeSet<String> = cores
            .values()
            .filter(|c| c.relevant)
            .flat_map(|c| c.platform_ids.iter().cloned())
            .filter(|p| !self.skip_plat(p))
            .collect();
        let primary: HashSet<String> = cores
            .values()
            .filter_map(|c| c.platform_ids.first().cloned())
            .collect();
        if !is_dir("Platforms") {
            if !used.is_empty() {
                self.note("Notes", "No /Platforms folder - cores will show no platform info".to_string(), &[]);
            }
            return;
        }
        let jsons: BTreeSet<String> = self
            .fs
            .listdir("Platforms")
            .into_iter()
            .filter(|f| ends_ci(f, ".json") && is_file(&format!("Platforms/{f}")))
            .map(|f| strip_chars(&f, 5))
            .collect();
        let imgs: BTreeSet<String> = self
            .fs
            .listdir("Platforms/_images")
            .into_iter()
            .filter(|f| ends_ci(f, ".bin"))
            .map(|f| strip_chars(&f, 4))
            .collect();
        let jl: HashMap<String, String> = jsons.iter().map(|j| (j.to_lowercase(), j.clone())).collect();
        let il: HashMap<String, String> = imgs.iter().map(|i| (i.to_lowercase(), i.clone())).collect();

        for pid in &used {
            let pj = format!("Platforms/{pid}.json");
            if !plat_id_ok(pid) {
                self.add(
                    "Platform problems",
                    format!("platform id '{pid}' breaks naming rules (a-z, 0-9, _; max 15; no leading _)"),
                    ps![pj],
                );
            }
            if jsons.contains(pid) {
                match read_json(&pj) {
                    Err(e) => self.add("Platform problems", format!("{pj}: {e}"), ps![pj]),
                    Ok(data) => match data.get("platform") {
                        Some(p) if p.is_object() => {
                            for k in ["category", "name", "manufacturer"] {
                                match p.get(k).and_then(|v| v.as_str()) {
                                    Some(v) if !v.is_empty() => {
                                        let n = v.chars().count();
                                        if n > 31 {
                                            self.add("Platform problems", format!("{pj}: '{k}' is {n} chars (max 31)"), ps![pj]);
                                        }
                                    }
                                    _ => self.add("Platform problems", format!("{pj}: '{k}' missing or not text"), ps![pj]),
                                }
                            }
                            let year_ok = p.get("year").map(|y| y.is_i64() || y.is_u64()).unwrap_or(false);
                            if !year_ok {
                                self.add("Platform problems", format!("{pj}: 'year' missing or not an integer"), ps![pj]);
                            }
                        }
                        _ => self.add("Platform problems", format!("{pj}: no 'platform' object"), ps![pj]),
                    },
                }
            } else if let Some(actual) = jl.get(&pid.to_lowercase()) {
                self.add(
                    "Case mismatches",
                    format!("Platforms/{actual}.json should be '{pid}.json'"),
                    ps![format!("Platforms/{actual}.json")],
                );
            } else {
                let users: Vec<&str> = cores
                    .iter()
                    .filter(|(_, c)| c.platform_ids.contains(pid))
                    .map(|(n, _)| n.as_str())
                    .collect();
                let msg = format!("{pj} missing (used by: {})", users.join(", "));
                if primary.contains(pid) {
                    self.add("Platform problems", msg, ps![pj]);
                } else {
                    // secondary platform (e.g. jtpatreon) - only used for optional shared assets
                    self.note("Platform JSON missing for secondary platform (optional)", msg, ps![pj]);
                }
            }

            let img = format!("Platforms/_images/{pid}.bin");
            if !imgs.contains(pid) {
                match il.get(&pid.to_lowercase()) {
                    None => self.note("Platform images missing (optional)", img.clone(), ps![img]),
                    Some(actual) => self.add(
                        "Case mismatches",
                        format!("Platforms/_images/{actual}.bin should be '{pid}.bin'"),
                        ps![format!("Platforms/_images/{actual}.bin")],
                    ),
                }
            }
        }

        if self.filt.is_empty() {
            // the unused-platform check only makes sense for the whole card
            let all_used: HashSet<String> = cores.values().flat_map(|c| c.platform_ids.iter().cloned()).collect();
            let unused_jsons: Vec<String> = jsons
                .iter()
                .filter(|j| !all_used.contains(*j) && !self.skip_plat(j))
                .cloned()
                .collect();
            for j in unused_jsons {
                let p = format!("Platforms/{j}.json");
                self.add("Platform JSONs no installed core uses", p.clone(), ps![p]);
            }
            let unused_imgs: Vec<String> = imgs
                .iter()
                .filter(|i| !all_used.contains(*i) && !self.skip_plat(i))
                .cloned()
                .collect();
            for i in unused_imgs {
                let p = format!("Platforms/_images/{i}.bin");
                self.note("Platform images no installed core uses", p.clone(), ps![p]);
            }
        }
    }

    // -------------------------------------------------------------- PRESETS
    /// Presets/<Core>/<Interact|Input>/<path under Assets of the instance json>
    fn check_presets(&mut self, cores: &BTreeMap<String, CoreInfo>, inst_rel: &BTreeSet<String>) {
        if !is_dir("Presets") {
            self.note("Notes", "No /Presets folder".to_string(), &[]);
            return;
        }
        let kinds = [("Interact", "interact"), ("Input", "input")];
        let installed: HashSet<String> = cores.keys().map(|n| n.to_lowercase()).collect();
        let mut have: HashMap<(String, String), HashSet<String>> = HashMap::new();

        for core in self.fs.listdir("Presets") {
            let cdir = format!("Presets/{core}");
            if !is_dir(&cdir) || self.ign.core(&core) {
                continue;
            }
            if !installed.contains(&core.to_lowercase()) {
                self.note("Presets folders with no installed core", cdir.clone(), ps![cdir]);
            }
            for kind in self.fs.listdir(&cdir) {
                let kdir = format!("{cdir}/{kind}");
                if !is_dir(&kdir) {
                    continue;
                }
                let key = kinds.iter().find(|(k, _)| *k == kind).map(|(_, v)| *v);
                let walked = self.fs.walk(&kdir, &|_: &str| false);
                for (dirpath, files) in walked {
                    for fnm in files {
                        if !ends_ci(&fnm, ".json") {
                            continue;
                        }
                        let full = join(&dirpath, &fnm);
                        let relp = full[kdir.len() + 1..].to_string();
                        let first = relp.split('/').next().unwrap_or("").to_string();
                        if !self.matches(&first) || self.skip_plat(&first) {
                            continue;
                        }
                        match read_json(&full) {
                            Err(e) => {
                                self.add("Bad preset JSON", format!("{full}: {e}"), ps![full]);
                                continue;
                            }
                            Ok(data) => {
                                if let Some(k) = key {
                                    match data.get(k) {
                                        Some(body) if body.is_object() => {
                                            if str_of(body, "magic") != Some("APF_VER_1") {
                                                self.add(
                                                    "Bad preset JSON",
                                                    format!("{full}: magic is {}", repr(body.get("magic"))),
                                                    ps![full],
                                                );
                                            }
                                        }
                                        _ => self.add(
                                            "Bad preset JSON",
                                            format!("{full}: no top-level '{k}' object"),
                                            ps![full],
                                        ),
                                    }
                                }
                            }
                        }
                        have.entry((core.clone(), kind.clone())).or_default().insert(relp.clone());
                        if !inst_rel.contains(&relp) {
                            self.add("Orphan presets (no matching instance JSON)", full.clone(), ps![full]);
                        }
                    }
                }
            }
        }

        // instance JSONs in a core's own folder that lack a preset
        for inst in inst_rel {
            let parts: Vec<&str> = inst.split('/').collect();
            if parts.len() < 3 || !installed.contains(&parts[1].to_lowercase()) {
                continue;
            }
            let core = parts[1];
            for (kind, _) in kinds {
                let key = (core.to_string(), kind.to_string());
                let p = format!("Presets/{core}/{kind}");
                match have.get(&key) {
                    None => self.note("Cores with no presets of a kind", p.clone(), ps![p]),
                    Some(set) if set.is_empty() => self.note("Cores with no presets of a kind", p.clone(), ps![p]),
                    Some(set) => {
                        if !set.contains(inst) {
                            self.add(
                                "Instance JSONs missing a preset",
                                format!("{inst}  (expected {p}/{inst})"),
                                ps![format!("Assets/{inst}"), format!("{p}/{inst}")],
                            );
                        }
                    }
                }
            }
        }
    }

    // --------------------------------------------------------------- ASSETS
    /// Returns false when the json is not an instance json.
    fn check_instance(&mut self, rel_json: &str, plat_dir: &str, data: &Value, referenced: &mut HashSet<String>) -> bool {
        let inst = match data.get("instance") {
            Some(i) if i.is_object() => i,
            _ => return false,
        };
        if str_of(inst, "magic") != Some("APF_VER_1") {
            self.add(
                "Bad instance JSON",
                format!("{rel_json}: magic is {}, expected 'APF_VER_1'", repr(inst.get("magic"))),
                ps![rel_json],
            );
        }

        let raw_dp = str_of(inst, "data_path").unwrap_or("");
        if raw_dp.contains('\\') {
            self.add(
                "Bad instance JSON",
                format!("{rel_json}: data_path uses backslashes ('{raw_dp}'); forward slashes only"),
                ps![rel_json],
            );
        }
        if raw_dp.chars().count() > 255 {
            self.add("Bad instance JSON", format!("{rel_json}: data_path longer than 255 chars"), ps![rel_json]);
        }
        let data_path = raw_dp.replace('\\', "/").trim_matches('/').to_string();

        let slots: Vec<Value> = match inst.get("data_slots") {
            None => Vec::new(),
            Some(Value::Array(a)) => a.clone(),
            Some(_) => {
                self.add("Bad instance JSON", format!("{rel_json}: data_slots is not a list"), ps![rel_json]);
                return true;
            }
        };
        if slots.len() > 32 {
            self.add("Bad instance JSON", format!("{rel_json}: {} data_slots (max 32)", slots.len()), ps![rel_json]);
        }

        let json_dir = dirname(rel_json);
        let common_dir = format!("Assets/{plat_dir}/common");
        let mut seen_ids: HashSet<i64> = HashSet::new();
        // (wanted dir, card dir) -> [(wanted name, card name)]
        let mut case_hits: BTreeMap<(String, String), Vec<(String, String)>> = BTreeMap::new();

        for slot in &slots {
            let sid = to_int(slot.get("id"));
            let sid_s = opt_int(sid);
            match sid {
                Some(v) if (0..=0xFFFF).contains(&v) => {
                    if seen_ids.contains(&v) {
                        self.add("Bad instance JSON", format!("{rel_json}: duplicate slot id {v}"), ps![rel_json]);
                    }
                    seen_ids.insert(v);
                }
                _ => self.add(
                    "Bad instance JSON",
                    format!("{rel_json}: invalid slot id {}", repr(slot.get("id"))),
                    ps![rel_json],
                ),
            }

            let fname = match str_of(slot, "filename") {
                Some(f) if !f.is_empty() => f.to_string(),
                _ => {
                    self.add("Bad instance JSON", format!("{rel_json}: slot {sid_s} has no filename"), ps![rel_json]);
                    continue;
                }
            };
            if fname.chars().count() > 255 {
                self.add("Bad instance JSON", format!("{rel_json}: slot {sid_s} filename longer than 255 chars"), ps![rel_json]);
            }
            if self.skip_file(&fname) {
                continue;
            }

            // Spec: files start in the instance JSON's folder (+ data_path). Fall back
            // to the platform's common folder for shared assets.
            let mut tries: Vec<String> = Vec::new();
            for base in [json_dir.as_str(), common_dir.as_str()] {
                if !data_path.is_empty() {
                    tries.push(normpath(&join(&join(base, &data_path), &fname)));
                }
                tries.push(normpath(&join(base, &fname)));
            }
            let mut uniq: Vec<String> = Vec::new();
            for t in tries {
                if !uniq.contains(&t) {
                    uniq.push(t);
                }
            }

            let mut hit: Option<(String, bool, String)> = None; // (found, case, the try that matched)
            for t in &uniq {
                let (found, case) = self.fs.find_exact("", t);
                if let Some(f) = found {
                    hit = Some((f, case, t.clone()));
                    break;
                }
            }
            let (found, case_issue, t) = match hit {
                Some(h) => h,
                None => {
                    let main_try = uniq
                        .iter()
                        .find(|t| t.starts_with(&format!("{common_dir}/")))
                        .unwrap_or(&uniq[0])
                        .clone(); // ROMs normally live in common/
                    let extra = if uniq.len() > 1 {
                        format!("; also tried {} other path(s)", uniq.len() - 1)
                    } else {
                        String::new()
                    };
                    let mut paths: Vec<&str> = uniq.iter().map(|s| s.as_str()).collect();
                    paths.push(rel_json);
                    self.add(
                        "Missing ROMs",
                        format!("{rel_json}  ->  {fname}   (slot {sid_s}; expected at {main_try}{extra})"),
                        &paths,
                    );
                    continue;
                }
            };

            referenced.insert(found.clone());
            if case_issue && !self.is_ignored(&[found.as_str(), t.as_str()]) {
                case_hits
                    .entry((dirname(&t), dirname(&found)))
                    .or_default()
                    .push((basename(&t).to_string(), basename(&found).to_string()));
            }

            let size = file_size(&found);
            if size == 0 {
                self.add("Empty ROMs", found.clone(), ps![found]);
            }
            if let Some(exp) = self.manifest.get(&basename(&fname).to_lowercase()).copied() {
                if size != exp {
                    self.add("Size/CRC mismatches", format!("{found}: size {size} != expected {exp}"), ps![found]);
                }
            }
        }

        // one line per folder pair, showing exactly which path the JSON asks for vs what is on the card
        for ((wd, fd), names) in case_hits {
            let files: Vec<String> = names
                .iter()
                .map(|(w, f)| if w == f { w.clone() } else { format!("{w} (card: {f})") })
                .collect();
            let head = if wd != fd {
                format!("JSON asks for folder '{wd}/' but card has '{fd}/'")
            } else {
                format!("filename case differs in '{wd}/'")
            };
            self.add(
                "Case mismatches",
                format!("{rel_json}: {head}  [{} file(s): {}]", names.len(), files.join(", ")),
                ps![fd, wd, rel_json],
            );
        }
        true
    }
}

// ------------------------------------------------------------------- run
pub fn run(opts: &Opts, inputs: Inputs) -> Result<Output, String> {
    if !is_dir("Assets") {
        return Err("No 'Assets' folder found - is the Pocket SD card root available to the plugin?".to_string());
    }

    let mut skip_files: HashSet<String> = ALWAYS_SKIP_FILES.iter().map(|s| s.to_string()).collect();
    let mut skip_plats: HashSet<String> = ALWAYS_SKIP_PLATFORMS.iter().map(|s| s.to_string()).collect();
    if !opts.include_analogizer {
        skip_files.extend(ANALOGIZER_FILES.iter().map(|s| s.to_string()));
        skip_plats.extend(ANALOGIZER_PLATFORMS.iter().map(|s| s.to_string()));
    }
    let filt: Vec<String> = opts.platforms.iter().map(|p| p.to_lowercase()).collect();
    let n_entries = inputs.ignore_entries.len();

    let mut sc = Scan {
        fs: Fs::new(),
        ign: IgnoreList::new(inputs.ignore_entries),
        items: Cats::default(),
        notes: Cats::default(),
        ignored: BTreeMap::new(),
        skip_files,
        skip_plats,
        ignore_sav: !opts.include_sav,
        filt: filt.clone(),
        manifest: inputs.manifest,
    };
    if let Some(n) = &inputs.created_note {
        sc.notes.push("Notes", n.clone());
    }

    let mut n_json = 0usize;
    let mut n_inst = 0usize;
    let mut n_rom = 0usize;
    let mut inst_platforms: BTreeSet<String> = BTreeSet::new();
    let mut inst_rel: BTreeSet<String> = BTreeSet::new();
    let mut referenced: HashSet<String> = HashSet::new();

    crate::say("Checking Cores, Platforms...");
    let cores = sc.scan_cores();
    sc.check_platforms(&cores);
    let used_by_kept: HashSet<String> = cores
        .values()
        .filter(|c| !c.ignored)
        .flat_map(|c| c.platform_ids.iter().map(|p| p.to_lowercase()))
        .collect();
    let ignored_platforms: HashSet<String> = cores
        .values()
        .filter(|c| c.ignored)
        .flat_map(|c| c.platform_ids.iter().map(|p| p.to_lowercase()))
        .filter(|p| !used_by_kept.contains(p))
        .collect();

    for plat in sc.fs.listdir("Assets") {
        let pdir = format!("Assets/{plat}");
        if !is_dir(&pdir)
            || !sc.matches(&plat)
            || ignored_platforms.contains(&plat.to_lowercase())
            || sc.skip_plat(&plat)
        {
            continue;
        }
        crate::say(&format!("Checking {pdir} ..."));
        let walked = sc.fs.walk(&pdir, &|n| sc.ign.core(n));
        for (dirpath, files) in walked {
            for fnm in files {
                if !ends_ci(&fnm, ".json") || sc.skip_file(&fnm) {
                    continue;
                }
                n_json += 1;
                let jp = join(&dirpath, &fnm);
                match read_json(&jp) {
                    Err(e) => sc.add("Bad instance JSON", format!("{jp}: {e}"), ps![jp]),
                    Ok(data) => {
                        if sc.check_instance(&jp, &plat, &data, &mut referenced) {
                            n_inst += 1;
                            inst_platforms.insert(plat.clone());
                            inst_rel.insert(jp["Assets/".len()..].to_string());
                        }
                    }
                }
            }
        }
    }

    // platforms that cores use but that have no Assets folder at all
    let asset_dirs: HashSet<String> = sc.fs.listdir("Assets").iter().map(|d| d.to_lowercase()).collect();
    for (name, c) in &cores {
        if !c.relevant {
            continue;
        }
        for (i, pid) in c.platform_ids.iter().enumerate() {
            if sc.skip_plat(pid) {
                continue;
            }
            if !asset_dirs.contains(&pid.to_lowercase()) {
                let msg = format!("Cores/{name}: platform '{pid}' has no /Assets/{pid} folder");
                let (a, b) = (format!("Assets/{pid}"), format!("Cores/{name}"));
                if i == 0 {
                    sc.add("Core folder problems", msg, ps![a, b]);
                } else {
                    sc.note("Secondary platforms with no Assets folder (optional)", msg, ps![a, b]);
                }
            }
        }
    }

    crate::say("Checking Presets...");
    sc.check_presets(&cores, &inst_rel);

    // orphan ROMs - only in platforms that actually use instance JSONs
    crate::say("Checking for orphan ROMs...");
    for plat in &inst_platforms {
        let walked = sc.fs.walk(&format!("Assets/{plat}"), &|n| sc.ign.core(n));
        for (dirpath, files) in walked {
            for fnm in files {
                if ROM_EXTS.iter().any(|e| ends_ci(&fnm, e)) && !sc.skip_file(&fnm) {
                    n_rom += 1;
                    let full = join(&dirpath, &fnm);
                    if !referenced.contains(&full) {
                        sc.add("Orphan ROMs (no JSON uses them)", full.clone(), ps![full]);
                    }
                }
            }
        }
    }

    let n_ign = cores.values().filter(|c| c.ignored).count();
    for (n, c) in &cores {
        if c.ignored {
            sc.ignored.entry("Ignored cores".to_string()).or_default().insert(n.clone());
        }
    }
    if filt.is_empty() {
        // with a platform filter many entries can't match, so skip the check
        for raw in sc.ign.unmatched() {
            sc.notes.push("Ignore list entries that matched nothing", raw);
        }
    }

    // ------------------------------------------------------------ report
    let mut skipping: Vec<&str> = Vec::new();
    if !opts.include_analogizer {
        skipping.push("Analogizer/CRT config");
    }
    skipping.push("jtpatreon");
    skipping.push("coinopkey");
    if !opts.include_sav {
        skipping.push("*.sav");
    }
    skipping.push("._* files");

    let mut lines: Vec<String> = vec![
        "Scanned Pocket SD card".to_string(),
        format!("  Skipping: {}", skipping.join(", ")),
        match &inputs.ignore_src {
            Some(src) => format!("  Ignore list: {src} ({n_entries} entries)"),
            None => "  Ignore list: none".to_string(),
        },
        format!(
            "  {} cores ({n_ign} ignored), {n_json} JSON files under Assets, {n_inst} instance JSONs, {n_rom} ROM files checked for orphans",
            cores.len()
        ),
    ];
    if !filt.is_empty() {
        lines.push(format!("  Platform filter: {}", filt.join(", ")));
    }
    lines.push(String::new());

    let push_cat = |lines: &mut Vec<String>, cat: &str, msgs: &[String]| {
        lines.push(format!("== {cat} ({}) ==", msgs.len()));
        let uniq: BTreeSet<&String> = msgs.iter().collect();
        for m in uniq {
            lines.push(format!("  {m}"));
        }
        lines.push(String::new());
    };
    let mut categories = Vec::new();
    for (cat, msgs) in &sc.items.0 {
        push_cat(&mut lines, cat, msgs);
        categories.push((cat.clone(), msgs.len()));
    }
    if !sc.notes.0.is_empty() {
        lines.push("---- Notes (not counted as problems) ----".to_string());
        for (cat, msgs) in &sc.notes.0 {
            push_cat(&mut lines, cat, msgs);
        }
    }
    if !sc.ignored.is_empty() {
        let src = match &inputs.ignore_src {
            Some(s) => basename(s).to_string(),
            None => "the ignore list".to_string(),
        };
        lines.push(format!("---- Ignored via {src} (not counted as problems) ----"));
        let order = |t: &str| match t {
            "Ignored cores" => 0,
            "Ignored folders" => 1,
            _ => 2,
        };
        let mut titles: Vec<&String> = sc.ignored.keys().collect();
        titles.sort_by_key(|t| order(t.as_str()));
        for title in titles {
            let set = &sc.ignored[title];
            lines.push(format!("== {title} ({}) ==", set.len()));
            let mut v: Vec<&String> = set.iter().collect();
            v.sort_by_key(|s| s.to_lowercase());
            for m in v {
                lines.push(format!("  {m}"));
            }
            lines.push(String::new());
        }
    }
    let problems = sc.items.total();
    lines.push(if problems == 0 {
        "No problems found.".to_string()
    } else {
        format!("{problems} problem(s) found.")
    });

    Ok(Output { text: lines.join("\n"), problems, categories })
}
