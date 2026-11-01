//! netmeter: de meetbank van HopOS op de host. Hij meet tegen `apps/bench`
//! op een node (een IP en de gepubliceerde poort) wat de tabellen van
//! docs/measurements.md vragen: TCP-doorvoer de node in en uit, de rtt
//! over N verbindingen, de verbindingscyclus en verbindingen per seconde,
//! en UDP.
//!
//! De Go-netmeter (`OLD/metal/cmd/netmeter`) was zelf de payload van de
//! node en trok van een host; deze staat op de host en meet de node van
//! buiten, zoals de Vitals-metingen van september. De uitvoer houdt de vorm
//! van Go (`NETMETER_BEGIN phase=...`, `NETMETER phase=... bytes= ms=
//! MBps=`, `NETMETER_FAIL`, `NETMETER_DONE`), zodat de regels naast de
//! oude te leggen zijn; wat eruit ging zijn de GC- en allocatietellers (de
//! node heeft geen GC meer, en zijn eigen tellers staan in `HOPOS_TICK`).
//! MB/s is decimaal, zoals in measurements.md (Go rekende MiB/s).
//!
//! ```text
//! netmeter 192.168.1.50:9000                      alle fasen, één keer
//! netmeter 127.0.0.1:8081 --phases rtt,out --repeat 3
//! netmeter NODE:PORT --conns 4 --bytes 67108864 --json > run.json
//! ```
//!
//! Met `--json` gaan de regels naar stderr en komt op stdout één object met
//! elke uitkomst (de rij voor measurements.md). De exitcode is 0 als elke
//! fase slaagde.

mod phases;
mod proto;

use phases::{Outcome, Plan};
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

/// De fasen, in de volgorde waarin ze standaard lopen: eerst de kleine
/// (rtt, storm), dan de doorvoer, dan UDP.
const PHASES: [&str; 5] = ["rtt", "storm", "in", "out", "udp"];

/// De hulptekst.
const USAGE: &str = "usage: netmeter NODE:PORT [--phases rtt,storm,in,out,udp] [--conns N] \
[--rtt N] [--storm N] [--bytes N] [--udp N] [--repeat N] [--label TEXT] [--json]";

/// De opties.
#[derive(Clone, Debug)]
struct Opts {
    plan: Plan,
    phases: Vec<String>,
    repeat: usize,
    label: String,
    json: bool,
}

/// Leest de argumenten. Een fout is de hulptekst met de reden.
fn parse(args: &[String]) -> Result<Opts, String> {
    let mut it = args.iter();
    let mut target: Option<String> = None;
    let mut o = Opts {
        plan: Plan {
            addr: SocketAddr::from(([127, 0, 0, 1], 9000)),
            conns: 1,
            rtt: 200,
            storm: 200,
            bytes: 64 << 20,
            udp: 200,
        },
        phases: PHASES.iter().map(|s| (*s).to_owned()).collect(),
        repeat: 1,
        label: String::new(),
        json: false,
    };
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        let num = |v: String| {
            v.parse::<u64>()
                .map_err(|_| format!("{a}: {v:?} is not a number"))
        };
        match a.as_str() {
            "--json" => o.json = true,
            "--phases" => o.phases = val()?.split(',').map(str::to_owned).collect(),
            "--conns" => o.plan.conns = num(val()?)?.clamp(1, 64) as usize,
            "--rtt" => o.plan.rtt = num(val()?)?.max(1) as usize,
            "--storm" => o.plan.storm = num(val()?)?.max(1) as usize,
            "--bytes" => o.plan.bytes = num(val()?)?.max(1),
            "--udp" => o.plan.udp = num(val()?)?.max(1) as usize,
            "--repeat" => o.repeat = num(val()?)?.clamp(1, 100) as usize,
            "--label" => o.label = val()?,
            "-h" | "--help" => return Err(String::new()),
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s => target = Some(s.to_owned()),
        }
    }
    let t = target.ok_or("no NODE:PORT")?;
    o.plan.addr = t
        .to_socket_addrs()
        .map_err(|e| format!("{t}: {e}"))?
        .next()
        .ok_or(format!("{t}: no address"))?;
    if let Some(p) = o.phases.iter().find(|p| !PHASES.contains(&p.as_str())) {
        return Err(format!("unknown phase {p}"));
    }
    Ok(o)
}

/// Draait één fase.
fn run_phase(name: &str, plan: &Plan) -> io::Result<Outcome> {
    match name {
        "rtt" => phases::rtt(plan),
        "storm" => phases::storm(plan),
        "in" => phases::inbound(plan),
        "out" => phases::outbound(plan),
        _ => phases::udp(plan),
    }
}

/// Een regel naar de gekozen stroom: stderr met `--json` (stdout is dan het
/// object), anders stdout.
fn say(json: bool, line: &str) {
    if json {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let o = match parse(&args) {
        Ok(o) => o,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("netmeter: {e}");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let p = &o.plan;
    say(
        o.json,
        &format!(
            "NETMETER host {} {}/{} against {} conns={} rtt={} storm={} bytes={} udp={} repeat={}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            p.addr,
            p.conns,
            p.rtt,
            p.storm,
            p.bytes,
            p.udp,
            o.repeat
        ),
    );
    let mut done: Vec<(usize, Outcome)> = Vec::new();
    let mut failed: Vec<(usize, String, String)> = Vec::new();
    for round in 1..=o.repeat {
        for name in &o.phases {
            say(o.json, &format!("NETMETER_BEGIN phase={name} run={round}"));
            match run_phase(name, p) {
                Ok(out) => {
                    say(o.json, &format!("{} run={round}", out.line()));
                    done.push((round, out));
                }
                Err(e) => {
                    say(
                        o.json,
                        &format!("NETMETER_FAIL phase={name} run={round} err={e}"),
                    );
                    failed.push((round, name.clone(), e.to_string()));
                }
            }
        }
    }
    for name in &o.phases {
        if let Some(s) = summary(name, &done) {
            say(o.json, &s);
        }
    }
    say(
        o.json,
        &format!("NETMETER_DONE ok={} failed={}", done.len(), failed.len()),
    );
    if o.json {
        println!("{}", json(&o, &done, &failed));
    }
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// De spreiding over de herhalingen: min en max van het hoofdgetal
/// (measurements.md toont de spreiding, niet alleen de beste run).
fn summary(name: &str, done: &[(usize, Outcome)]) -> Option<String> {
    let runs: Vec<&Outcome> = done
        .iter()
        .filter(|(_, o)| o.phase == name)
        .map(|(_, o)| o)
        .collect();
    if runs.len() < 2 {
        return None;
    }
    let (what, vals): (&str, Vec<f64>) = match name {
        "in" | "out" => ("MBps", runs.iter().map(|o| o.mbps).collect()),
        "storm" => (
            "conn_per_s",
            runs.iter().filter_map(|o| o.conn_per_s).collect(),
        ),
        _ => (
            "p50_us",
            runs.iter()
                .filter_map(|o| o.lat_us.map(|l| l[1] as f64))
                .collect(),
        ),
    };
    let min = vals.iter().copied().fold(f64::INFINITY, f64::min);
    let max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Some(format!(
        "NETMETER_SPREAD phase={name} {what}={min:.2}..{max:.2} runs={}",
        vals.len()
    ))
}

/// Een JSON-string (alleen wat hier voorkomt: aanhalingstekens,
/// backslashes en stuurtekens).
fn js(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out += "\\\"",
            '\\' => out += "\\\\",
            c if (c as u32) < 0x20 => out += &format!("\\u{:04x}", c as u32),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Het object voor measurements.md: de opzet en elke uitkomst.
fn json(o: &Opts, done: &[(usize, Outcome)], failed: &[(usize, String, String)]) -> String {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let p = &o.plan;
    let runs: Vec<String> = done
        .iter()
        .map(|(r, x)| {
            let mut f = vec![
                format!("\"run\":{r}"),
                format!("\"phase\":{}", js(x.phase)),
                format!("\"bytes\":{}", x.bytes),
                format!("\"ms\":{}", x.ms),
                format!("\"MBps\":{:.3}", x.mbps),
            ];
            if let Some(n) = x.node_mbps {
                f.push(format!("\"node_MBps\":{n:.3}"));
            }
            if let Some([min, p50, p90, p99, max]) = x.lat_us {
                f.push(format!(
                    "\"us\":{{\"min\":{min},\"p50\":{p50},\"p90\":{p90},\"p99\":{p99},\"max\":{max}}}"
                ));
            }
            if let Some(c) = x.conn_per_s {
                f.push(format!("\"conn_per_s\":{c:.1}"));
            }
            if x.phase == "udp" || x.phase == "out" {
                f.push(format!("\"lost\":{}", x.lost));
            }
            format!("{{{}}}", f.join(","))
        })
        .collect();
    let fails: Vec<String> = failed
        .iter()
        .map(|(r, n, e)| format!("{{\"run\":{r},\"phase\":{},\"err\":{}}}", js(n), js(e)))
        .collect();
    format!(
        "{{\"tool\":\"netmeter\",\"version\":{},\"unix\":{unix},\"label\":{},\"target\":{},\"conns\":{},\"rtt\":{},\"storm\":{},\"bytes\":{},\"udp\":{},\"repeat\":{},\"results\":[{}],\"failed\":[{}]}}",
        js(env!("CARGO_PKG_VERSION")),
        js(&o.label),
        js(&p.addr.to_string()),
        p.conns,
        p.rtt,
        p.storm,
        p.bytes,
        p.udp,
        o.repeat,
        runs.join(","),
        fails.join(",")
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn options_parse() {
        let o = parse(&args(
            "127.0.0.1:8081 --phases rtt,out --conns 4 --json --repeat 3",
        ))
        .unwrap();
        assert_eq!(o.plan.addr, SocketAddr::from(([127, 0, 0, 1], 8081)));
        assert_eq!(o.phases, ["rtt", "out"]);
        assert_eq!((o.plan.conns, o.repeat, o.json), (4, 3, true));
        assert!(parse(&args("--json")).is_err());
        assert!(parse(&args("1.2.3.4:5 --phases rtt,warp")).is_err());
        assert!(parse(&args("1.2.3.4:5 --conns x")).is_err());
    }

    #[test]
    fn json_is_escaped() {
        assert_eq!(js("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    }

    #[test]
    fn the_spread_needs_two_runs() {
        let o = |m| Outcome {
            phase: "out",
            mbps: m,
            ..Outcome::default()
        };
        assert!(summary("out", &[(1, o(1.0))]).is_none());
        let s = summary("out", &[(1, o(40.0)), (2, o(45.5))]).unwrap();
        assert_eq!(s, "NETMETER_SPREAD phase=out MBps=40.00..45.50 runs=2");
    }
}
