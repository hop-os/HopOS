//! loc: telt de regels van HopOS zoals de compiler ze ziet. Per release-smaak
//! één `cargo build` met het échte target en de échte features, en uit de
//! dep-info (`.d`) van elke gelinkte crate de bronbestanden, zodat een module
//! die maar voor één board of één ISA bouwt ook alleen dáár meetelt. Geen
//! cloc-schatting over de hele boom: wat niet gelinkt wordt, telt niet.
//!
//! ```text
//! cargo run -p loc                 de emmers
//! cargo run -p loc -- -v           plus de files per emmer
//! ```
//!
//! De Go-meter (`OLD/tools/loc.go`) deed hetzelfde met `go list -deps` per
//! GOOS/GOARCH/tags; de emmers zijn dezelfde, in de volgorde van de ladder
//! op de site:
//!   - portable: in élke smaak gelinkt, per laag (kern, node, drivers, net,
//!     firmware & boot)
//!   - per ISA: in alle boards van één architectuur, board-onafhankelijk
//!   - per board: wat alleen déze smaak linkt, board-support plus zijn
//!     drivers; dít is de swappable buitenlaag
//!   - lean: de eigen stdlib, nu een git-dependency op een tag, apart zoals
//!     toen (eigen repo, eigen versie); een andere crate van buiten zou hier
//!     als `extern/` opduiken
//!   - gui: het verschil tussen `board-x,gui` en kaal; telt in geen kale
//!     node mee
//!
//! Nieuw tegenover Go is de kolom toetsen: Go had zijn toetsen in `_test.go`
//! die `go list` niet noemde, Rust heeft ze in hetzelfde bestand onder
//! `#[cfg(test)]`, dus de teller splitst ze af (count.rs) en de ladder telt
//! alleen productiecode. Build-scripts tellen niet (bouwtijd, geen image), en
//! wat de node via `include_bytes!` inbakt is geen code. De dev-targets
//! (qemuvirt, qemuvirt-riscv, uefi) zijn geen smaak: ze bepalen de doorsnede
//! niet en krijgen geen emmer. Media is een opt-in zoals gui maar alleen op
//! de O6N; die laag staat hier niet.
//!
//! De builds lopen het bouwpad van de image-scripts (release, en voor de
//! UEFI-boards PIE in `target/uefi`, de Radxa in de zijne), zodat de meter
//! hun cache deelt; na een `tools/gate.sh` is alles vers en is dit seconden.

mod count;
mod json;

use count::Count;
use json::Json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::{env, fs};

const AARCH64: &str = "aarch64-unknown-none-softfloat";
const RISCV64: &str = "riscv64gc-unknown-none-elf";
const PIE: Option<&str> = Some("-C relocation-model=pie");

/// Eén release-smaak: de kern-binary zoals het image-script hem bouwt.
struct Flavour {
    name: &'static str,
    /// De ISA-laag waar het board in valt.
    arch: &'static str,
    target: &'static str,
    feature: &'static str,
    /// Heeft dit board een gui-smaak (dezelfde features plus `gui`)?
    gui: bool,
    /// Het bouwpad van het image: andere RUSTFLAGS of een eigen target-map,
    /// zodat de meter de cache van het image deelt en niet weggooit.
    rustflags: Option<&'static str>,
    target_dir: Option<&'static str>,
}

const fn flavour(name: &'static str, feature: &'static str, gui: bool) -> Flavour {
    Flavour {
        name,
        arch: "arm64",
        target: AARCH64,
        feature,
        gui,
        rustflags: None,
        target_dir: None,
    }
}

/// De smaken, in de volgorde van de ladder. De Pi's en de Mac bouwen in de
/// gedeelde target-map (image/rpi4.sh, rpi5.sh, apple-m4.sh), de Radxa in
/// target/radxa-zero3/cargo (radxa-zero3.sh), de UEFI-boards als PIE in
/// target/uefi (uefi-run.sh), de LicheeRV op riscv64 (licheerv-agent.sh).
const FLAVOURS: [Flavour; 7] = [
    flavour("rpi4", "board-rpi4", true),
    flavour("rpi5", "board-rpi5", true),
    Flavour {
        target_dir: Some("target/radxa-zero3/cargo"),
        ..flavour("rk3566", "board-rk3566", true)
    },
    Flavour {
        rustflags: PIE,
        target_dir: Some("target/uefi"),
        ..flavour("o6n", "board-o6n", true)
    },
    Flavour {
        rustflags: PIE,
        target_dir: Some("target/uefi"),
        ..flavour("altra", "board-altra", true)
    },
    flavour("apple", "board-apple", false),
    Flavour {
        arch: "riscv64",
        target: RISCV64,
        ..flavour("licheerv", "board-licheerv", false)
    },
];

/// De ladder-sporten van een portable file, op het eerste pad-deel.
const LAYERS: [(&str, &[&str]); 5] = [
    (
        "isolation core (abi, kern, cpu, sync, executor, heap, bounded)",
        &["abi", "kern", "cpu", "sync", "executor", "heap", "bounded"],
    ),
    ("node main (hopos)", &["hopos"]),
    (
        "drivers (dev, netdev, blkdev, driver)",
        &["dev", "netdev", "blkdev", "driver"],
    ),
    ("network stack (net)", &["net"]),
    ("firmware & boot (fw, board, gui)", &["fw", "board", "gui"]),
];

/// De sport van een portable file.
fn layer(key: &str) -> String {
    let first = key.split('/').next().unwrap_or("");
    LAYERS
        .iter()
        .find(|(_, dirs)| dirs.contains(&first))
        .map_or_else(
            || format!("overig ({first})"),
            |(label, _)| (*label).to_owned(),
        )
}

/// Waar een crate vandaan komt, aan zijn package-id.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Deze werkruimte (`path+file://...`).
    Ours,
    /// De lean-repo (`git+https://github.com/xinix00/lean.git?tag=...`).
    Lean,
    /// Al het andere: hoort er niet te zijn (handboek §8), dus zichtbaar.
    Extern,
}

fn origin_of(package_id: &str) -> Origin {
    if package_id.starts_with("path+") {
        Origin::Ours
    } else if package_id.contains("xinix00/lean") {
        Origin::Lean
    } else {
        Origin::Extern
    }
}

/// Een crate van buiten de werkruimte: lean of extern.
fn is_foreign(key: &str) -> bool {
    key.starts_with("lean/") || key.starts_with("extern/")
}

/// De sleutel van een bronbestand: het pad vanaf de wortel voor de onze,
/// `lean/<crate>/<pad in de crate>` voor lean, `extern/<crate>/...` voor
/// de rest.
fn key_of(origin: Origin, krate: &str, root: &Path, manifest_dir: &Path, abs: &Path) -> String {
    let rel = |base: &Path| {
        abs.strip_prefix(base)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    };
    let in_crate = || rel(manifest_dir).unwrap_or_else(|| abs.display().to_string());
    match origin {
        Origin::Ours => rel(root).unwrap_or_else(|| format!("extern/{krate}/{}", in_crate())),
        Origin::Lean => format!("lean/{krate}/{}", in_crate()),
        Origin::Extern => format!("extern/{krate}/{}", in_crate()),
    }
}

/// Sleutel naar het echte pad.
type Files = BTreeMap<String, PathBuf>;

/// De wortel van de werkruimte, waar cargo de dep-info-paden aan relateert.
fn workspace_root() -> Result<PathBuf, String> {
    let out = Command::new("cargo")
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .output()
        .map_err(|e| format!("cargo locate-project: {e}"))?;
    if !out.status.success() {
        return Err("cargo locate-project failed".to_owned());
    }
    let manifest = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    manifest
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "no workspace root".to_owned())
}

/// De dep-info naast een artefact: `libabi-HASH.rlib` heeft `abi-HASH.d`,
/// de binary `hopos` heeft `hopos.d`.
fn dep_info_path(artifact: &str) -> PathBuf {
    let p = Path::new(artifact);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let stem = stem.strip_prefix("lib").unwrap_or(stem);
    p.with_file_name(format!("{stem}.d"))
}

/// De Rust-bronnen uit een dep-info: de eerste regel `doel: bron bron ...`.
fn sources(dep: &Path) -> Result<Vec<PathBuf>, String> {
    let text = fs::read_to_string(dep).map_err(|e| format!("{}: {e}", dep.display()))?;
    let rule = text
        .lines()
        .find_map(|l| l.split_once(": "))
        .ok_or_else(|| format!("{}: no rule", dep.display()))?;
    Ok(rule
        .1
        .split_whitespace()
        .filter(|f| f.ends_with(".rs"))
        .map(PathBuf::from)
        .collect())
}

/// Bouwt één smaak (met of zonder gui) en geeft de bronbestanden die erin
/// gelinkt zijn, uit de dep-info van elke crate die cargo noemt.
fn files_of(root: &Path, fl: &Flavour, gui: bool) -> Result<Files, String> {
    let mut features = fl.feature.to_owned();
    if gui {
        features.push_str(",gui");
    }
    eprintln!("== loc: cargo build {} ({features})", fl.name);
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "--release",
        "--target",
        fl.target,
        "-p",
        "hopos",
        "--features",
        &features,
    ])
    .arg("--message-format=json-render-diagnostics")
    .stderr(Stdio::inherit());
    if let Some(d) = fl.target_dir {
        cmd.args(["--target-dir", d]);
    }
    if let Some(f) = fl.rustflags {
        cmd.env("RUSTFLAGS", f);
    }
    let out = cmd.output().map_err(|e| format!("cargo build: {e}"))?;
    if !out.status.success() {
        return Err(format!("cargo build {} ({features}) failed", fl.name));
    }
    let mut files = Files::new();
    let mut seen = BTreeSet::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let m = json::parse(line)?;
        if m.get("reason").and_then(Json::str) != Some("compiler-artifact") {
            continue;
        }
        let target = m.get("target").ok_or("artifact without target")?;
        let kinds = target.get("kind").and_then(Json::arr).unwrap_or(&[]);
        if kinds.iter().any(|k| k.str() == Some("custom-build")) {
            continue;
        }
        let name = target.get("name").and_then(Json::str).unwrap_or("?");
        let origin = origin_of(m.get("package_id").and_then(Json::str).unwrap_or(""));
        let manifest_dir = Path::new(m.get("manifest_path").and_then(Json::str).unwrap_or(""))
            .parent()
            .unwrap_or(Path::new(""));
        let artifacts = m.get("filenames").and_then(Json::arr).unwrap_or(&[]);
        let Some(dep) = artifacts
            .iter()
            .filter_map(Json::str)
            .map(dep_info_path)
            .find(|p| p.is_file())
        else {
            return Err(format!("{name}: no dep-info next to its artifacts"));
        };
        if !seen.insert(dep.clone()) {
            continue;
        }
        for src in sources(&dep)? {
            let abs = if src.is_absolute() {
                src
            } else {
                root.join(src)
            };
            files.insert(key_of(origin, name, root, manifest_dir, &abs), abs);
        }
    }
    Ok(files)
}

fn count_file(path: &Path) -> Result<Count, String> {
    let src = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(count::rust(&src))
}

/// Per emmer de telling, en per emmer de files erin (voor `-v`).
#[derive(Default)]
struct Buckets {
    sum: BTreeMap<String, Count>,
    files: BTreeMap<String, Vec<(String, Count)>>,
}

impl Buckets {
    fn add(&mut self, bucket: &str, key: &str, n: Count) {
        *self.sum.entry(bucket.to_owned()).or_default() += n;
        self.files
            .entry(bucket.to_owned())
            .or_default()
            .push((key.to_owned(), n));
    }
}

fn row(label: &str, c: Count) -> String {
    format!("  {label:<62} {:>7} {:>7}", c.code, c.tests)
}

fn run(verbose: bool) -> Result<(), String> {
    let root = workspace_root()?;
    env::set_current_dir(&root).map_err(|e| format!("{}: {e}", root.display()))?;

    let mut sets: BTreeMap<&str, Files> = BTreeMap::new();
    let mut all = Files::new();
    for fl in &FLAVOURS {
        let set = files_of(&root, fl, false)?;
        all.extend(set.iter().map(|(k, v)| (k.clone(), v.clone())));
        sets.insert(fl.name, set);
    }

    // De gui-smaak, apart: per board het verschil tussen `gui` en kaal.
    // Deze files tellen in geen enkele emmer hierboven mee: de kale node is
    // de basis, dit is de opt-in laag erbovenop.
    let gui_boards: Vec<&str> = FLAVOURS.iter().filter(|f| f.gui).map(|f| f.name).collect();
    let mut gui_sets: BTreeMap<&str, Files> = BTreeMap::new();
    let mut gui_all = Files::new();
    for fl in FLAVOURS.iter().filter(|f| f.gui) {
        let diff: Files = files_of(&root, fl, true)?
            .into_iter()
            .filter(|(k, _)| !sets[fl.name].contains_key(k))
            .collect();
        gui_all.extend(diff.iter().map(|(k, v)| (k.clone(), v.clone())));
        gui_sets.insert(fl.name, diff);
    }

    let mut lines: BTreeMap<&str, Count> = BTreeMap::new();
    for (k, p) in all.iter().chain(gui_all.iter()) {
        lines.insert(k, count_file(p)?);
    }

    // Per file: in welke smaken zit hij? Daaruit volgt de emmer.
    let arm64: Vec<&str> = FLAVOURS
        .iter()
        .filter(|f| f.arch == "arm64")
        .map(|f| f.name)
        .collect();
    let mut portable: BTreeMap<String, Count> = BTreeMap::new();
    let mut arch_common: BTreeMap<&str, Count> = BTreeMap::new();
    let mut board: BTreeMap<String, Count> = BTreeMap::new();
    let mut foreign: BTreeMap<String, Count> = BTreeMap::new();
    let mut node_total: BTreeMap<&str, Count> = BTreeMap::new();
    let mut lean_total: BTreeMap<&str, Count> = BTreeMap::new();
    let mut buckets = Buckets::default();
    for fl in &FLAVOURS {
        for k in sets[fl.name].keys() {
            let total = if is_foreign(k) {
                &mut lean_total
            } else {
                &mut node_total
            };
            *total.entry(fl.name).or_default() += lines[k.as_str()];
        }
    }
    for k in all.keys() {
        let present: Vec<&str> = FLAVOURS
            .iter()
            .filter(|f| sets[f.name].contains_key(k))
            .map(|f| f.name)
            .collect();
        let n = lines[k.as_str()];
        let everywhere = present.len() == FLAVOURS.len();
        let bucket = if is_foreign(k) {
            let sig = if everywhere {
                "alle smaken".to_owned()
            } else {
                present.join("+")
            };
            *foreign.entry(sig.clone()).or_default() += n;
            format!("lean · {sig}")
        } else if everywhere {
            *portable.entry(layer(k)).or_default() += n;
            format!("portable · {}", layer(k))
        } else if present == arm64 {
            *arch_common.entry("arm64").or_default() += n;
            "arm64-common".to_owned()
        } else if present == ["licheerv"] && !k.starts_with("board/") && !k.starts_with("driver/") {
            // Eén riscv64-board, dus arch en board vallen samen; het pad
            // splitst ze: cpu/kern/dev is de ISA-laag, board+driver het bord.
            *arch_common.entry("riscv64").or_default() += n;
            "riscv64-common".to_owned()
        } else {
            let sig = present.join("+");
            *board.entry(sig.clone()).or_default() += n;
            format!("board {sig}")
        };
        buckets.add(&bucket, k, n);
    }

    // De gui-laag: gemeenschappelijk (fbgrant, usbin, usb, xhci, hid) versus
    // board-eigen (rkscan en de scanout-bedrading alleen op de rk3566).
    let mut gui_total: BTreeMap<&str, Count> = BTreeMap::new();
    let mut gui_buckets: BTreeMap<String, Count> = BTreeMap::new();
    for k in gui_all.keys() {
        let n = lines[k.as_str()];
        let present: Vec<&str> = gui_boards
            .iter()
            .copied()
            .filter(|b| gui_sets[b].contains_key(k))
            .collect();
        for b in &present {
            *gui_total.entry(b).or_default() += n;
        }
        let sig = if present.len() == gui_boards.len() {
            "alle gui-boards".to_owned()
        } else {
            present.join("+")
        };
        *gui_buckets.entry(sig.clone()).or_default() += n;
        buckets.add(&format!("gui · {sig}"), k, n);
    }

    println!("{:<64} {:>7} {:>7}", "", "regels", "toetsen");
    println!("== portable: telt voor élke node ==");
    let mut total = Count::default();
    for (label, _) in &LAYERS {
        let n = portable.get(*label).copied().unwrap_or_default();
        println!("{}", row(label, n));
        total += n;
    }
    for (label, n) in portable.iter().filter(|(l, _)| l.starts_with("overig")) {
        println!("{}  (indelen!)", row(label, *n));
        total += *n;
    }
    println!("{}", row("portable totaal", total));

    println!("\n== per ISA: board-onafhankelijk ==");
    let arch = |a: &str| arch_common.get(a).copied().unwrap_or_default();
    println!("{}", row("arm64 (alle arm64-boards)", arch("arm64")));
    println!("{}", row("riscv64", arch("riscv64")));

    println!("\n== per board: de swappable buitenlaag ==");
    for (sig, n) in &board {
        println!("{}", row(sig, *n));
    }

    println!("\n== lean: de eigen stdlib (git, op een tag): wat de node linkt ==");
    for (sig, n) in &foreign {
        println!("{}", row(sig, *n));
    }
    // Per crate: het antwoord op "linkt de node X eigenlijk wel?"
    let mut per_crate: BTreeMap<String, Count> = BTreeMap::new();
    for (k, n) in all
        .keys()
        .filter(|k| is_foreign(k))
        .map(|k| (k, lines[k.as_str()]))
    {
        let krate: Vec<&str> = k.splitn(3, '/').take(2).collect();
        *per_crate.entry(krate.join("/")).or_default() += n;
    }
    for (krate, n) in &per_crate {
        println!("  {}", row(krate, *n));
    }

    println!("\n== gui: de opt-in smaak (feature gui), telt in geen kale node mee ==");
    for (sig, n) in &gui_buckets {
        println!("{}", row(sig, *n));
    }

    // De ladder-view: een node = portable + zijn ISA-laag + zijn board-laag.
    // De board-laag is de rest van wat déze machine linkt, inclusief zijn
    // NIC/PHY/display-drivers en wat hij met een buurbord deelt. Alleen
    // productiecode; de gui-kolom is de aparte vermelding.
    println!("\n== een node = portable + ISA-laag + board-laag (gui apart; productiecode) ==");
    for fl in &FLAVOURS {
        let node = node_total.get(fl.name).copied().unwrap_or_default().code;
        let isa = arch(fl.arch).code;
        let lean = lean_total.get(fl.name).copied().unwrap_or_default().code;
        let gui = if fl.gui {
            format!(
                "   (+{} met gui)",
                gui_total.get(fl.name).copied().unwrap_or_default().code
            )
        } else {
            String::new()
        };
        println!(
            "  {:<10} {:>6} + {:>5} ({}) + {:>5} (board) = {:>6} eigen + {:>5} lean{gui}",
            fl.name,
            total.code,
            isa,
            fl.arch,
            node - total.code - isa,
            node,
            lean,
        );
    }

    if verbose {
        println!("\n== files per emmer ==");
        for (bucket, files) in &buckets.files {
            println!("{bucket}");
            for (k, n) in files {
                println!("  {:>6} {:>6}  {k}", n.code, n.tests);
            }
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let verbose = match args.as_slice() {
        [] => false,
        [v] if v == "-v" => true,
        _ => {
            eprintln!("usage: loc [-v]");
            return ExitCode::from(2);
        }
    };
    match run(verbose) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("loc: {e}");
            ExitCode::FAILURE
        }
    }
}
