//! Builds the compact index from MFT scans, applying exclusions (PLAN.md §5).

use std::collections::{HashMap, HashSet};

use rayon::prelude::*;
use whereti_ntfs::mft::MftScan;
use whereti_query::fold;

use crate::index::*;

/// File attributes that mark cloud-only placeholders.
const ATTR_OFFLINE: u32 = 0x1000;
const ATTR_RECALL_ON_OPEN: u32 = 0x0004_0000;
const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
const ATTR_HIDDEN: u32 = 0x2;
const ATTR_SYSTEM: u32 = 0x4;

/// NTFS keeps its own metadata in records 0-15 ($MFT, $LogFile, $Extend, …).
const FIRST_USER_RECORD: u64 = 16;
const ROOT_RECORD: u64 = 5;

pub struct VolumeScan {
    pub letter: char,
    pub serial: u64,
    pub journal_id: u64,
    /// Journal position read before the scan started.
    pub next_usn: i64,
    pub scan: MftScan,
}

/// First component of a `**\name` rule (that name at any depth).
pub(crate) const ANY_DEPTH: &str = "**";

/// Paths left out of the name index. `C:\Windows` applies to drive C only;
/// `\System Volume Information` (no letter) applies to every volume;
/// `**\node_modules` leaves out every `node_modules`, at any depth.
///
/// Folders that are generated can also be told by what is around them, so
/// a name that people use too (`build`, `target`, `Library`) is left out only
/// where a tool made it:
/// - `**\go\pkg\mod`: that path, ending anywhere (`*` = any one folder:
///   `**\Packages\*\LocalCache`);
/// - `**\AppData\...\Code Cache`: that folder anywhere below an `AppData`
///   (a name that only tools use there, and people may use elsewhere);
/// - `**\*.lrdata`: folders whose name ends so;
/// - `**\target <Cargo.toml>`: `target` beside a `Cargo.toml` (`|` between
///   several, `*.csproj` for any of a kind);
/// - `**\<CACHEDIR.TAG>`: a folder holding that file or folder.
#[derive(Debug, Clone, Default)]
pub struct Exclusions {
    pub paths: Vec<String>,
}

/// Folders that hold only generated files nobody looks for: left out of
/// the index entirely by default (`**\name` rules). On a developer PC
/// `node_modules` alone can be half of all entries.
pub const GENERATED_DIRS: &[&str] = &[
    // version control internals
    ".git", ".svn", ".hg", ".bzr",
    // JavaScript / web
    "node_modules", "bower_components", ".npm", ".pnpm-store", ".next", ".nuxt", ".svelte-kit", ".angular",
    ".parcel-cache", ".turbo", ".vercel", ".expo", ".docusaurus", ".sass-cache",
    // Python
    "__pycache__", ".venv", ".tox", ".mypy_cache", ".pytest_cache", ".ruff_cache", ".ipynb_checkpoints", ".eggs",
    // Java / Kotlin / .NET / C++ / Dart / infra
    ".gradle", ".m2", ".nuget", ".vs", "ipch", "cmake-build-debug", "cmake-build-release", ".dart_tool", ".pub-cache",
    ".terraform", ".serverless",
];

/// The folders left out by default because they are generated, in groups
/// the app shows (and lets people take back) in Settings, Search. Only rules
/// that cannot catch a person's own folder: a tool's marker, a build folder
/// next to its project file, a tool's own path or a name no one else uses.
pub const DEFAULT_GROUPS: &[(&str, &[&str])] = &[
    (
        // what programmers' tools generate
        "dev",
        &[
            r"**\target <Cargo.toml>",
            r"**\build <build.gradle|build.gradle.kts|settings.gradle|CMakeLists.txt|pubspec.yaml|meson.build>",
            r"**\bin <*.csproj|*.fsproj|*.vbproj|*.vcxproj>",
            r"**\obj <*.csproj|*.fsproj|*.vbproj|*.vcxproj>",
            r"**\dist <package.json|pyproject.toml|setup.py>",
            r"**\.output <package.json>",
            r"**\out <package.json>",
            r"**\vendor <go.mod|composer.json|Gemfile>",
            r"**\Pods <Podfile>",
            r"**\.build <Package.swift>",
            r"**\_build <mix.exs|dune-project>",
            r"**\deps <mix.exs>",
            r"**\zig-cache <build.zig>",
            r"**\.zig-cache <build.zig>",
        ],
    ),
    (
        // caches and environments that mark themselves, and toolchains'
        // download stores
        "caches",
        &[
            r"**\<CACHEDIR.TAG>",
            r"**\<pyvenv.cfg>",
            r"**\<conda-meta>",
            r"**\go\pkg\mod",
            r"**\AppData\Local\go-build",
            r"**\.cargo\registry",
            r"**\.cargo\git",
            r"**\.rustup\toolchains",
            r"**\AppData\Local\pip\cache",
            r"**\AppData\Local\npm-cache",
            r"**\AppData\Local\Yarn\Cache",
            r"**\AppData\Local\pnpm\store",
            r"**\AppData\Local\NuGet\v3-cache",
            r"**\AppData\Local\ms-playwright",
            r"**\AppData\Local\electron\Cache",
            r"**\.android\cache",
            r"**\.vscode\extensions",
            r"**\.cursor\extensions",
        ],
    ),
    (
        // previews and render caches of photo, video and audio programs
        "media",
        &[
            r"**\*.lrdata",
            r"**\Adobe\Common\Media Cache Files",
            r"**\Adobe\Common\Media Cache",
            r"**\Adobe Premiere Pro Video Previews",
            r"**\Adobe Premiere Pro Audio Previews",
            r"**\Adobe After Effects Disk Cache",
            r"**\CacheClip",
            r"**\CaptureOne\Cache",
        ],
    ),
    (
        // what applications keep for themselves: temporary files, browser
        // profiles, the caches of Chromium and Electron apps and of editors,
        // Store apps' caches, logs and crash dumps. Nobody looks for these by
        // name; every rule is held to AppData, ProgramData or the place the
        // program itself uses, so a "Cache" or "logs" of one's own stays.
        "apps",
        &[
            r"**\AppData\Local\Temp",
            r"**\AppData\Local\Google\Chrome\User Data",
            r"**\AppData\Local\Google\Chrome Beta\User Data",
            r"**\AppData\Local\Microsoft\Edge\User Data",
            r"**\AppData\Local\BraveSoftware\Brave-Browser\User Data",
            r"**\AppData\Local\Vivaldi\User Data",
            r"**\AppData\Local\Chromium\User Data",
            r"**\AppData\Roaming\Opera Software",
            r"**\AppData\Local\Opera Software",
            r"**\AppData\Roaming\Mozilla\Firefox\Profiles",
            r"**\AppData\Local\Mozilla\Firefox\Profiles",
            r"**\AppData\Roaming\Thunderbird\Profiles",
            r"**\AppData\Local\Microsoft\Windows\INetCache",
            r"**\AppData\Local\Microsoft\Windows\WebCache",
            r"**\AppData\Local\Microsoft\Windows\Explorer",
            r"**\AppData\Local\CrashDumps",
            r"**\AppData\Local\D3DSCache",
            r"**\AppData\Local\NVIDIA\DXCache",
            r"**\AppData\Local\NVIDIA\GLCache",
            r"**\AppData\Local\AMD\DxCache",
            r"**\AppData\Local\Packages\*\LocalCache",
            r"**\AppData\Local\Packages\*\TempState",
            r"**\AppData\Local\Packages\*\AC",
            r"**\AppData\...\Cache",
            r"**\AppData\...\Code Cache",
            r"**\AppData\...\GPUCache",
            r"**\AppData\...\DawnCache",
            r"**\AppData\...\DawnGraphiteCache",
            r"**\AppData\...\DawnWebGPUCache",
            r"**\AppData\...\GrShaderCache",
            r"**\AppData\...\ShaderCache",
            r"**\AppData\...\Service Worker",
            r"**\AppData\...\IndexedDB",
            r"**\AppData\...\blob_storage",
            r"**\AppData\...\Session Storage",
            r"**\AppData\...\Local Storage",
            r"**\AppData\...\Crashpad",
            r"**\AppData\...\CachedData",
            r"**\AppData\...\CachedExtensionVSIXs",
            r"**\AppData\...\CachedProfilesData",
            r"**\AppData\...\User\History",
            r"**\AppData\...\User\workspaceStorage",
            r"**\AppData\...\caches",
            r"**\AppData\...\logs",
            r"**\AppData\...\log",
            r"**\ProgramData\...\logs",
            r"**\ProgramData\Package Cache",
            r"**\ProgramData\...\Cache",
            r"**\AppData\...\site-packages",
            r"**\AppData\Local\golangci-lint",
            r"**\AppData\Local\Steam\htmlcache",
            r"**\Steam\appcache",
            // the XDG cache folder some tools keep in the profile
            r"**\Users\*\.cache",
            r"**\Program Files\WindowsApps",
            r"**\.codex\.tmp",
        ],
    ),
    (
        // what SDKs and developer toolchains install: headers, libraries,
        // reference packages. On by default, a switch for who reads them.
        "sdks",
        &[
            r"**\Program Files (x86)\Windows Kits",
            r"**\Program Files\Windows Kits",
            r"**\Program Files (x86)\Microsoft SDKs",
            r"**\Program Files\Microsoft SDKs",
            r"**\Program Files\Microsoft Visual Studio",
            r"**\Program Files (x86)\Microsoft Visual Studio",
            r"**\Program Files\dotnet\packs",
            r"**\Program Files\dotnet\sdk",
            r"**\Program Files\dotnet\shared",
            r"**\Program Files\Go\src",
            r"**\Program Files\Go\pkg",
            r"**\Android\android-sdk",
            r"**\AppData\Local\Android\Sdk",
            r"**\Google\Cloud SDK\google-cloud-sdk",
            r"**\msys64",
            r"**\msys2",
            r"**\cygwin64",
            r"**\Strawberry\perl",
            r"**\Strawberry\c",
            r"**\ProgramData\Microsoft\VisualStudio",
        ],
    ),
    (
        // what game engines generate inside a project
        "games",
        &[
            r"**\Library <ProjectSettings>",
            r"**\Temp <ProjectSettings>",
            r"**\Intermediate <*.uproject|*.uplugin>",
            r"**\DerivedDataCache <*.uproject>",
            r"**\.godot <project.godot>",
        ],
    ),
];

/// A rule that looks around a folder, not only at its path (see `Exclusions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Smart {
    /// Folded components; the last one is the folder. `*` matches any one.
    Tail(Vec<String>),
    /// `tail` (as in `Tail`) somewhere below a folder named `anchor`.
    Under { anchor: String, tail: Vec<String> },
    /// Folded name ending, with its dot (`.lrdata`).
    Suffix(String),
    /// A folder `name` beside one of `manifests` (folded; `*.ext` = any of a kind).
    Beside { name: String, manifests: Vec<String> },
    /// A folder holding a file or folder with this folded name.
    Marker(String),
}

impl Smart {
    /// Parses the text after `**\`. `None` for a plain name (or nonsense).
    pub(crate) fn parse(rest: &str) -> Option<Smart> {
        let rest = rest.trim();
        if let Some(marker) = rest.strip_prefix('<').and_then(|r| r.strip_suffix('>')) {
            let m = marker.trim();
            return (!m.is_empty() && !m.contains(['\\', '*', '|'])).then(|| Smart::Marker(fold(m)));
        }
        if let Some((name, list)) = rest.split_once(" <") {
            let list = list.strip_suffix('>')?;
            let manifests: Vec<String> = list.split('|').map(str::trim).filter(|m| !m.is_empty()).map(fold).collect();
            let name = name.trim();
            let ok = !name.is_empty()
                && !name.contains(['\\', '*'])
                && !manifests.is_empty()
                && manifests.iter().all(|m| !m.contains('\\') && (!m.contains('*') || (m.starts_with("*.") && m.len() > 2 && !m[1..].contains('*'))));
            return ok.then(|| Smart::Beside { name: fold(name), manifests });
        }
        if let Some(suffix) = rest.strip_prefix('*') {
            let ok = suffix.starts_with('.') && suffix.len() > 1 && !suffix.contains(['\\', '*', '<']);
            return ok.then(|| Smart::Suffix(fold(suffix)));
        }
        // components of a path: names, or `*` for any one folder (never the last)
        let comps = |text: &str| -> Option<Vec<String>> {
            let comps: Vec<String> = text.split('\\').map(str::trim).map(fold).collect();
            let ok = comps.iter().all(|c| !c.is_empty() && (c == "*" || !c.contains(['*', '<', '>', '|'])))
                && comps.last().is_some_and(|c| c != "*");
            ok.then_some(comps)
        };
        if let Some((anchor, tail)) = rest.split_once("\\...\\") {
            let anchor = anchor.trim();
            if anchor.is_empty() || anchor.contains(['\\', '*', '<', '>', '|']) {
                return None;
            }
            return Some(Smart::Under { anchor: fold(anchor), tail: comps(tail)? });
        }
        if rest.contains('\\') {
            let comps = comps(rest)?;
            return (comps.len() >= 2).then_some(Smart::Tail(comps));
        }
        None
    }
}

/// A folded name test that allocates only for non-ASCII names.
fn name_is(name: &str, folded_target: &str, buf: &mut String) -> bool {
    if name.is_ascii() {
        name.eq_ignore_ascii_case(folded_target)
    } else {
        buf.clear();
        whereti_query::fold_into(name, buf);
        buf == folded_target
    }
}

/// `name` folded into `buf` (ASCII lowercased without a second pass).
fn folded<'a>(name: &str, buf: &'a mut String) -> &'a str {
    buf.clear();
    if name.is_ascii() {
        buf.extend(name.chars().map(|c| c.to_ascii_lowercase()));
    } else {
        whereti_query::fold_into(name, buf);
    }
    buf.as_str()
}

impl Exclusions {
    /// The defaults from PLAN.md §5. `windows_dir` is `%SystemRoot%`
    /// (e.g. `C:\Windows`); pass `None` to keep it in the index.
    pub fn defaults(windows_dir: Option<&str>) -> Self {
        let mut e = Self::system_defaults(windows_dir);
        e.paths.extend(GENERATED_DIRS.iter().map(|d| format!("**\\{d}")));
        e.paths.extend(Self::group_rules());
        e
    }

    /// Every rule of `DEFAULT_GROUPS`, in order.
    pub fn group_rules() -> Vec<String> {
        DEFAULT_GROUPS.iter().flat_map(|(_, rules)| rules.iter().map(|r| r.to_string())).collect()
    }

    /// The defaults of whereti 0.0.2 to 0.9.2: system paths and generated
    /// folder names, without `DEFAULT_GROUPS`.
    pub fn defaults_v2(windows_dir: Option<&str>) -> Self {
        let mut e = Self::system_defaults(windows_dir);
        e.paths.extend(GENERATED_DIRS.iter().map(|d| format!("**\\{d}")));
        e
    }

    /// Whether `rule` is one whereti understands (a path or a `**\` rule).
    pub fn is_valid_any_depth(rule: &str) -> bool {
        match rule.strip_prefix("**\\") {
            Some(rest) if !rest.is_empty() => {
                Smart::parse(rest).is_some() || !rest.contains(['\\', '*', '<', '>', '|'])
            }
            _ => false,
        }
    }

    /// The rules that look around a folder, with their place in `paths`.
    pub(crate) fn smart(&self) -> Vec<(usize, Smart)> {
        self.paths
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.replace('/', "\\").strip_prefix("**\\").and_then(Smart::parse).map(|s| (i, s)))
            .collect()
    }

    /// The defaults before generated folders were left out (whereti ≤ 0.0.1
    /// builds of 2026-10-24): an index built with exactly these is upgraded.
    pub fn previous_defaults(windows_dir: Option<&str>) -> Self {
        Self::system_defaults(windows_dir)
    }

    fn system_defaults(windows_dir: Option<&str>) -> Self {
        let mut paths: Vec<String> = [
            r"\$Recycle.Bin",
            r"\System Volume Information",
            r"\$WinREAgent",
            r"\pagefile.sys",
            r"\hiberfil.sys",
            r"\swapfile.sys",
            r"\ProgramData\Microsoft\Windows\WER",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(w) = windows_dir {
            paths.push(w.to_string());
        }
        Exclusions { paths }
    }

    /// Rules for one volume, each as folded path components.
    /// `**\name` rules come out as `["**", name]`: that name anywhere.
    /// The rules that look around a folder are not here (see `smart`).
    pub(crate) fn for_volume(&self, letter: char) -> Vec<Vec<String>> {
        self.for_volume_ids(letter).into_iter().map(|(_, r)| r).collect()
    }

    /// `for_volume`, each rule with its place in `paths`.
    pub(crate) fn for_volume_ids(&self, letter: char) -> Vec<(usize, Vec<String>)> {
        let mut out = Vec::new();
        for (id, p) in self.paths.iter().enumerate() {
            let p = p.replace('/', "\\");
            if let Some(name) = p.strip_prefix("**\\") {
                if !name.is_empty() && !name.contains(['\\', '*', '<', '>', '|']) {
                    out.push((id, vec![ANY_DEPTH.to_string(), fold(name)]));
                }
                continue;
            }
            let rest = match p.as_bytes() {
                [l, b':', b'\\', ..] if (*l as char).eq_ignore_ascii_case(&letter) => &p[3..],
                [_, b':', ..] => continue,
                [b'\\', ..] => &p[1..],
                _ => continue,
            };
            let comps: Vec<String> = rest.split('\\').filter(|c| !c.is_empty()).map(fold).collect();
            if !comps.is_empty() {
                out.push((id, comps));
            }
        }
        out
    }
}

#[derive(Debug, Default, Clone)]
pub struct BuildStats {
    pub scanned: usize,
    pub excluded: usize,
    pub kept: usize,
}

pub fn build(volumes: Vec<VolumeScan>, exclusions: &Exclusions) -> (NameIndex, BuildStats) {
    let mut idx = NameIndex {
        exts: vec![String::new()],
        name_off: vec![0],
        alt_off: vec![0],
        exclusions: exclusions.paths.clone(),
        ..Default::default()
    };
    let mut ext_ids: HashMap<String, u16> = HashMap::new();
    let mut stats = BuildStats::default();
    let mut skipped = Vec::new();
    for vs in volumes {
        let letter = vs.letter.to_ascii_uppercase();
        let mut counts = vec![0u64; exclusions.paths.len()];
        add_volume(&mut idx, &mut ext_ids, vs, exclusions, &mut stats, &mut counts);
        for (rule, n) in exclusions.paths.iter().zip(counts) {
            if n > 0 {
                skipped.push((letter, rule.clone(), n));
            }
        }
    }
    idx.skipped = crate::index::SkipCounts(skipped);
    idx.name_rank = compute_name_rank(&idx);
    idx.dir_order = compute_dir_order(&idx);
    mark_noise(&mut idx);
    stats.kept = idx.len();
    (idx, stats)
}

fn add_volume(
    idx: &mut NameIndex,
    ext_ids: &mut HashMap<String, u16>,
    vs: VolumeScan,
    exclusions: &Exclusions,
    stats: &mut BuildStats,
    counts: &mut [u64],
) {
    let scan = &vs.scan;
    let n = scan.entries.len();
    stats.scanned += n;
    let max_rec = scan.entries.iter().map(|e| e.record + 1).max().unwrap_or(0) as usize;

    // record -> first entry with that record (folders have exactly one)
    let mut rec_first = vec![NO_PARENT; max_rec];
    for (i, e) in scan.entries.iter().enumerate() {
        if rec_first[e.record as usize] == NO_PARENT {
            rec_first[e.record as usize] = i as u32;
        }
    }
    let parent_of = |i: usize| -> u32 {
        let p = scan.entries[i].parent as usize;
        if p < max_rec { rec_first[p] } else { NO_PARENT }
    };
    let root = if (ROOT_RECORD as usize) < max_rec { rec_first[ROOT_RECORD as usize] } else { NO_PARENT };

    // 1. Seeds: NTFS metadata files and resolved exclusion paths. `rule`
    // remembers which exclusion left an entry out (its place + 1), for the
    // counts Settings shows.
    let mut state = vec![0u8; n]; // 0 = unknown, 1 = excluded, 2 = kept
    let mut rule = vec![0u32; n];
    for (i, e) in scan.entries.iter().enumerate() {
        if e.record < FIRST_USER_RECORD && e.record != ROOT_RECORD {
            state[i] = 1;
        }
    }
    let seed = |state: &mut [u8], rule: &mut [u32], i: usize, id: usize| {
        if i as u32 != root && state[i] != 1 {
            state[i] = 1;
            rule[i] = id as u32 + 1;
        }
    };
    let rules = vs_rules(exclusions, vs.letter);
    let (anywhere, paths): (Vec<_>, Vec<_>) = rules.into_iter().partition(|(_, r)| r[0] == ANY_DEPTH);
    if root != NO_PARENT {
        state[root as usize] = 2;
        for (i, id) in resolve_paths(scan, root, &paths, &parent_of) {
            seed(&mut state, &mut rule, i as usize, id);
        }
    }
    // `**\name`: every file or folder with that name, at any depth.
    if !anywhere.is_empty() {
        let mut folded = String::new();
        for (i, e) in scan.entries.iter().enumerate() {
            let name = scan.name(e);
            if let Some((id, _)) = anywhere.iter().find(|(_, r)| name_is(name, &r[1], &mut folded)) {
                seed(&mut state, &mut rule, i, *id);
            }
        }
    }
    // Rules that look around a folder.
    let smart = exclusions.smart();
    if !smart.is_empty() {
        let mut buf = String::new();
        let mut up = String::new();
        // folders holding a marker, and the project files beside each folder
        let mut beside: HashSet<(u32, usize)> = HashSet::new();
        for (i, e) in scan.entries.iter().enumerate() {
            let p = parent_of(i);
            if p == NO_PARENT {
                continue;
            }
            let name = folded(scan.name(e), &mut buf);
            for (k, (id, s)) in smart.iter().enumerate() {
                match s {
                    Smart::Marker(m) if name == m => seed(&mut state, &mut rule, p as usize, *id),
                    Smart::Beside { manifests, .. } => {
                        let hit = manifests.iter().any(|m| match m.strip_prefix('*') {
                            Some(ext) => name.ends_with(ext) && name.len() > ext.len(),
                            None => name == m,
                        });
                        if hit {
                            beside.insert((p, k));
                        }
                    }
                    _ => {}
                }
            }
        }
        // Whether a folder named `name` (in parent `p`) ends `comps`, the
        // folders above matching nearest first (`*` = any); the folder above
        // the match when it does.
        let tail_above = |comps: &[String], name: &str, p: u32, up: &mut String| -> Option<u32> {
            let (last, above) = comps.split_last()?;
            if name != last {
                return None;
            }
            let mut cur = p;
            for c in above.iter().rev() {
                if cur == NO_PARENT || cur == root {
                    return None;
                }
                if c != "*" && folded(scan.name(&scan.entries[cur as usize]), up) != c {
                    return None;
                }
                cur = parent_of(cur as usize);
            }
            Some(cur)
        };
        for (i, e) in scan.entries.iter().enumerate() {
            if !e.is_dir {
                continue;
            }
            let p = parent_of(i);
            let name = folded(scan.name(e), &mut buf).to_string();
            for (k, (id, s)) in smart.iter().enumerate() {
                let hit = match s {
                    Smart::Suffix(sfx) => name.ends_with(sfx.as_str()) && name.len() > sfx.len(),
                    Smart::Beside { name: want, .. } => p != NO_PARENT && name == *want && beside.contains(&(p, k)),
                    Smart::Tail(comps) => tail_above(comps, &name, p, &mut up).is_some(),
                    Smart::Under { anchor, tail } => tail_above(tail, &name, p, &mut up).is_some_and(|mut cur| {
                        // a folder named `anchor` somewhere above
                        for _ in 0..4096 {
                            if cur == NO_PARENT || cur == root {
                                return false;
                            }
                            if folded(scan.name(&scan.entries[cur as usize]), &mut up) == anchor {
                                return true;
                            }
                            cur = parent_of(cur as usize);
                        }
                        false
                    }),
                    Smart::Marker(_) => false,
                };
                if hit {
                    seed(&mut state, &mut rule, i, *id);
                    break;
                }
            }
        }
    }
    // 2. Every entry inherits the state of its nearest decided ancestor.
    let mut chain = Vec::new();
    for i in 0..n {
        if state[i] != 0 {
            continue;
        }
        chain.clear();
        let mut cur = i as u32;
        let (decided, by) = loop {
            if state[cur as usize] != 0 {
                break (state[cur as usize], rule[cur as usize]);
            }
            chain.push(cur);
            let p = parent_of(cur as usize);
            if p == NO_PARENT || p == cur || chain.len() > 4096 {
                break (2, 0); // orphan or cycle: keep it
            }
            cur = p;
        };
        for &c in &chain {
            state[c as usize] = decided;
            rule[c as usize] = by;
        }
    }
    for i in 0..n {
        if state[i] == 1 && rule[i] > 0 {
            if let Some(c) = counts.get_mut(rule[i] as usize - 1) {
                *c += 1;
            }
        }
    }

    // 3. Copy kept entries into the index columns.
    let base = idx.len() as u32;
    let mut new_idx = vec![NO_PARENT; n];
    let mut next = base;
    for i in 0..n {
        if state[i] == 2 {
            new_idx[i] = next;
            next += 1;
        } else {
            stats.excluded += 1;
        }
    }
    // exact room for the kept entries: no doubling, no second copy (§52)
    let kept = (next - base) as usize;
    idx.names.reserve_exact(scan.names.len() + kept);
    idx.name_off.reserve_exact(kept);
    idx.parent.reserve_exact(kept);
    idx.flags.reserve_exact(kept);
    idx.ext.reserve_exact(kept);
    idx.size.reserve_exact(kept);
    idx.mtime.reserve_exact(kept);
    idx.volume.reserve_exact(kept);
    let vol_no = idx.volumes.len() as u8;
    let mut folded = String::new();
    for i in 0..n {
        if state[i] != 2 {
            continue;
        }
        let e = &scan.entries[i];
        let name = scan.name(e);
        let is_root = i as u32 == root;
        let mut flags = attr_flags(e.attributes, e.is_dir || is_root);
        if is_root {
            flags |= FLAG_ROOT;
        }
        if !name.is_ascii() {
            flags |= FLAG_NON_ASCII;
            folded.clear();
            whereti_query::fold_into(name, &mut folded);
            idx.alt_entry.push(new_idx[i]);
            idx.alt_names.extend_from_slice(folded.as_bytes());
            idx.alt_names.push(0);
            idx.alt_off.push(idx.alt_names.len() as u32);
        }
        idx.names.extend_from_slice(name.as_bytes());
        idx.names.push(0);
        idx.name_off.push(idx.names.len() as u32);
        let new_root = if root == NO_PARENT { NO_PARENT } else { new_idx[root as usize] };
        let p = parent_of(i);
        idx.parent.push(if is_root {
            NO_PARENT
        } else if p == NO_PARENT || new_idx[p as usize] == NO_PARENT {
            new_root // orphan: show it at the volume root
        } else {
            new_idx[p as usize]
        });
        idx.flags.push(flags);
        idx.ext.push(if e.is_dir { 0 } else { ext_id(ext_ids, &mut idx.exts, name) });
        idx.size.push(e.size);
        idx.mtime.push(filetime_to_minutes(e.modified));
        idx.volume.push(vol_no);
    }

    // record -> kept entries (a hard link can be kept while another is excluded)
    let mut rec_map = vec![NO_PARENT; max_rec];
    for i in 0..n {
        if state[i] != 2 {
            continue;
        }
        let r = scan.entries[i].record as usize;
        if rec_map[r] == NO_PARENT {
            rec_map[r] = new_idx[i];
        } else {
            idx.links.entry(rec_map[r]).or_default().push(new_idx[i]);
        }
    }
    idx.rec_maps.push(rec_map);
    idx.volumes.push(VolumeInfo {
        letter: vs.letter as u32,
        root: if root == NO_PARENT { NO_PARENT } else { new_idx[root as usize] },
        first: base,
        end: next,
        serial: vs.serial,
        journal_id: vs.journal_id,
        next_usn: vs.next_usn,
        // where this volume's record map starts when the maps are stored back to back
        rec_start: idx.rec_maps.iter().rev().skip(1).map(|m| m.len() as u64).sum(),
        rec_len: max_rec as u64,
    });
}

fn vs_rules(exclusions: &Exclusions, letter: char) -> Vec<(usize, Vec<String>)> {
    exclusions.for_volume_ids(letter)
}

/// Finds the entries named by exclusion rules, one tree level per pass.
fn resolve_paths(
    scan: &MftScan,
    root: u32,
    rules: &[(usize, Vec<String>)],
    parent_of: &dyn Fn(usize) -> u32,
) -> Vec<(u32, usize)> {
    let mut found = Vec::new();
    // (rule, depth reached) -> entry matched so far
    let mut frontier: Vec<(usize, u32)> = (0..rules.len()).map(|r| (r, root)).collect();
    let mut depth = 0;
    while !frontier.is_empty() {
        let mut wanted: HashMap<u32, Vec<usize>> = HashMap::new();
        for &(r, at) in &frontier {
            wanted.entry(at).or_default().push(r);
        }
        let mut next = Vec::new();
        let mut folded = String::new();
        for (i, e) in scan.entries.iter().enumerate() {
            let Some(rs) = wanted.get(&parent_of(i)) else { continue };
            folded.clear();
            whereti_query::fold_into(scan.name(e), &mut folded);
            for &r in rs {
                let (id, comps) = &rules[r];
                if comps[depth] == folded {
                    if depth + 1 == comps.len() {
                        found.push((i as u32, *id));
                    } else if e.is_dir {
                        next.push((r, i as u32));
                    }
                }
            }
        }
        frontier = next;
        depth += 1;
    }
    found
}

/// Index flags for NTFS file attributes.
pub(crate) fn attr_flags(attributes: u32, is_dir: bool) -> u16 {
    let mut flags = 0u16;
    if is_dir {
        flags |= FLAG_DIR;
    }
    if attributes & ATTR_HIDDEN != 0 {
        flags |= FLAG_HIDDEN;
    }
    if attributes & ATTR_SYSTEM != 0 {
        flags |= FLAG_SYSTEM;
    }
    if attributes & (ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) != 0 {
        flags |= FLAG_CLOUD;
    }
    flags
}

pub(crate) fn ext_id(ids: &mut HashMap<String, u16>, exts: &mut Vec<String>, name: &str) -> u16 {
    let Some(dot) = name.rfind('.') else { return 0 };
    let ext = &name[dot + 1..];
    if ext.is_empty() || ext.len() > 16 || ext.contains(' ') {
        return 0;
    }
    let ext = fold(ext);
    if let Some(&id) = ids.get(&ext) {
        return id;
    }
    if exts.len() >= u16::MAX as usize {
        return 0;
    }
    let id = exts.len() as u16;
    exts.push(ext.clone());
    ids.insert(ext, id);
    id
}

/// FILETIME (100 ns since 1601) -> minutes since 1601.
pub fn filetime_to_minutes(ft: u64) -> u32 {
    (ft / 600_000_000).min(u32::MAX as u64) as u32
}

/// Case- and accent-insensitive sort key of an entry.
pub(crate) fn sort_key(idx: &NameIndex, i: u32) -> &[u8] {
    if idx.flags[i as usize] & FLAG_NON_ASCII != 0 {
        idx.alt_name(i)
    } else {
        idx.name_bytes(i)
    }
}

pub(crate) fn compute_name_rank(idx: &NameIndex) -> Vec<u32> {
    let mut order: Vec<u32> = (0..idx.len() as u32).collect();
    order.par_sort_unstable_by(|&a, &b| {
        let (ka, kb) = (sort_key(idx, a), sort_key(idx, b));
        ka.iter()
            .map(u8::to_ascii_lowercase)
            .cmp(kb.iter().map(u8::to_ascii_lowercase))
            .then_with(|| idx.name_bytes(a).cmp(idx.name_bytes(b)))
            .then(a.cmp(&b))
    });
    let mut rank = vec![0u32; idx.len()];
    for (pos, &i) in order.iter().enumerate() {
        rank[i as usize] = pos as u32;
    }
    rank
}

/// Folders sorted by depth, so parents always come first.
pub(crate) fn compute_dir_order(idx: &NameIndex) -> Vec<u32> {
    let n = idx.len();
    let mut depth = vec![u16::MAX; n];
    let mut chain = Vec::new();
    for i in 0..n as u32 {
        if !idx.is_dir(i) || depth[i as usize] != u16::MAX {
            continue;
        }
        chain.clear();
        let mut cur = i;
        let mut d = loop {
            if depth[cur as usize] != u16::MAX {
                break depth[cur as usize];
            }
            chain.push(cur);
            match idx.parent(cur) {
                Some(p) if chain.len() < 4096 => cur = p,
                _ => break 0,
            }
        };
        for &c in chain.iter().rev() {
            d = d.saturating_add(1);
            depth[c as usize] = d;
        }
    }
    let mut dirs: Vec<u32> = (0..n as u32).filter(|&i| idx.is_dir(i)).collect();
    dirs.sort_by_key(|&i| (depth[i as usize], i));
    dirs
}

/// Folder names whose contents users rarely look for: ranked last, and their
/// files are never indexed by content (PLAN.md §4.3, §6). Unlike
/// `GENERATED_DIRS` these names are also used for people's own folders
/// ("Release", "build", "out"…), so the files stay findable by name.
const NOISE_DIRS: &[&str] = &[
    // system and programs
    "appdata", "programdata", "program files", "program files (x86)", "windowsapps", "windows.old", "$windows.~bt",
    "msocache", "steamapps",
    // build output
    "bin", "obj", "debug", "release", "build", "dist", "out", "target", "x64", "x86", "coverage", ".nyc_output",
    // caches and temporary files
    ".cache", "cache", "caches", "temp", "tmp", ".tmp", "logs",
    // developer toolchains and bundled libraries
    "msys64", "msys2", "mingw64", "mingw32", "cygwin", "cygwin64", "strawberry", "include", "vendor", "third_party",
    "third-party", "external", "sdk", "sdks",
    // tools and environments
    "venv", "site-packages", ".cargo", ".rustup", ".yarn", ".idea", ".vscode", ".vscode-server", ".codex", ".conda",
    "anaconda3", "miniconda3", ".docker", ".android", ".kotlin",
    // editor and tool extensions, test data, Windows' own stores
    "extensions", "testdata", "test-data", "fixtures", "__fixtures__", "__snapshots__", "winsxs", "$recycle.bin",
    "system volume information", "packages", "dist-packages",
    // generated folders (also left out of the index by default; ranked last if a user keeps them)
    "node_modules", "bower_components", ".git", ".svn", ".hg", ".npm", ".pnpm-store", ".next", ".nuxt", ".turbo",
    "__pycache__", ".venv", ".tox", ".mypy_cache", ".pytest_cache", ".gradle", ".m2", ".nuget", ".vs",
];

pub(crate) fn is_noise_dir(folded_name: &str) -> bool {
    NOISE_DIRS.contains(&folded_name)
}

/// The user's own folders, directly in their profile (`C:\Users\<name>\…`).
const HOME_DIRS: &[&str] =
    &["desktop", "documents", "downloads", "pictures", "videos", "music", "dropbox", "google drive", "icloud drive"];

/// FLAG_NOISE or FLAG_HOME for an entry, from its folded name and its parent
/// (whose flags must already be right). Files only inherit.
pub(crate) fn place_flags(idx: &NameIndex, parent: Option<u32>, folded: &str, is_dir: bool) -> u16 {
    let Some(p) = parent else { return 0 };
    let pf = idx.flags[p as usize];
    if pf & FLAG_NOISE != 0 {
        return FLAG_NOISE;
    }
    if !is_dir {
        return pf & FLAG_HOME;
    }
    let folded_name = |i: u32| {
        let mut s = String::new();
        whereti_query::fold_into(idx.name(i), &mut s);
        s
    };
    let is_root = |i: u32| idx.flags[i as usize] & FLAG_ROOT != 0;
    let noise = is_noise_dir(folded)
        // tool folders: .vscode, .claude, .antigravity, .gradle…
        || folded.starts_with('.')
        // Go's module cache (go\pkg\mod) and C:\Windows
        || (folded == "mod" && folded_name(p) == "pkg")
        || (folded == "windows" && is_root(p))
        // language runtimes installed at the root of a drive (C:\Python313…)
        || (is_root(p) && ["python", "ruby", "php", "jdk", "perl", "nodejs"].iter().any(|r| folded.starts_with(r)))
        || (is_root(p) && folded == "go");
    if noise {
        return FLAG_NOISE;
    }
    if pf & FLAG_HOME != 0 {
        return FLAG_HOME;
    }
    let in_profile = idx
        .parent(p)
        .is_some_and(|g| folded_name(g) == "users" && idx.parent(g).is_some_and(is_root));
    if in_profile && (HOME_DIRS.contains(&folded) || folded.starts_with("onedrive")) {
        FLAG_HOME
    } else {
        0
    }
}

/// Sets FLAG_NOISE and FLAG_HOME on every entry (and clears them elsewhere,
/// so a saved index follows the current lists).
pub(crate) fn mark_noise(idx: &mut NameIndex) {
    for f in &mut idx.flags {
        *f &= !(FLAG_NOISE | FLAG_HOME);
    }
    let mut folded = String::new();
    for k in 0..idx.dir_order.len() {
        let d = idx.dir_order[k];
        folded.clear();
        whereti_query::fold_into(idx.name(d), &mut folded);
        let place = place_flags(idx, idx.parent(d), &folded, true);
        idx.flags[d as usize] |= place;
    }
    for i in 0..idx.len() {
        if idx.flags[i] & FLAG_DIR == 0 {
            let place = place_flags(idx, idx.parent(i as u32), "", false);
            idx.flags[i] |= place;
        }
    }
}

/// Text and code a person writes, on top of the types `ext_weight` favours
/// (for "where you work": an unknown extension is a program's own file).
pub(crate) fn is_written_by_hand(ext: &str) -> bool {
    matches!(
        ext,
        "txt" | "md" | "csv" | "tsv" | "json" | "xml" | "html" | "htm" | "css" | "scss" | "js" | "mjs" | "ts" | "tsx"
            | "jsx" | "vue" | "svelte" | "astro" | "py" | "rs" | "go" | "java" | "kt" | "c" | "h" | "cpp" | "hpp"
            | "cc" | "cs" | "php" | "rb" | "swift" | "dart" | "lua" | "sql" | "sh" | "ps1" | "bat" | "yaml" | "yml"
            | "toml" | "ini" | "tex" | "rtf"
    )
}

/// A name a program made up: a UUID or a long run of hex digits
/// ("945d0bda-b0c0-4d90-99ee-d6bb23acfd51.sslo", "3f9a…e1.tmp").
pub(crate) fn looks_generated(name: &str) -> bool {
    let mut run = 0;
    for c in name.chars() {
        if c.is_ascii_hexdigit() {
            run += 1;
            if run >= 16 {
                return true;
            }
        } else if c != '-' {
            run = 0;
        }
    }
    false
}

/// How much a file type matters to a person (negative = more), by folded
/// extension: their documents, pictures, music, videos and programs first,
/// the files programs keep for themselves last.
pub(crate) fn ext_weight(ext: &str) -> i8 {
    match ext {
        // documents, pictures, music, videos and archives (whereti_query::kinds)
        e if whereti_query::kinds::category(e).is_some() => -4,
        // programs, shortcuts, drawings
        "exe" | "msi" | "lnk" | "url" | "dwg" | "skp" | "blend" => -4,
        // what programs keep for themselves
        "dll" | "sys" | "log" | "tmp" | "temp" | "dat" | "bin" | "cache" | "pdb" | "obj" | "o" | "lib" | "a"
        | "so" | "mui" | "cat" | "manifest" | "etl" | "idx" | "pack" | "map" | "lock" | "pyc" | "class" | "ldb"
        | "sst" | "blob" | "db-wal" | "db-shm" | "nupkg" | "old" | "bak" => 6,
        _ => 0,
    }
}
