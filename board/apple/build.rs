//! De ingebakken stage: `HOPOS_EMBED=<pad>` (image/apple-m4.sh `EMBED=`)
//! bakt een gestripte app-ELF in het kernimage, zodat een Mac die zonder
//! loader boot (kmutil, het rauwe bootobject) toch een bewoner heeft: Hop.
//! Go deed dit met `go:embed` (cmd/hopos-embed). Zonder de variabele is
//! het bestand leeg en is er niets ingebakken. Dezelfde variabele als de
//! LicheeRV (board/licheerv/build.rs, met de rol in `HOPOS_EMBED_ROLE`).
//!
//! En de dockchannel van een chip die we nog niet gemeten hebben:
//! `HOPOS_APPLE_DOCK=0x...` (image/apple-m4.sh `DOCK=`) wordt
//! `chip::DOCK_BUILD`. Het getal komt uit `ioreg -p IODeviceTree` in macOS
//! (`dockchannel-uart` reg + de `ranges` van `arm-io`), vóór de eerste boot:
//! de eerste regel van de kern hangt eraan, en die mag niet aan een boom
//! hangen (head.rs). Leeg = de waarde van de chip-tabel.

use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-env-changed=HOPOS_EMBED");
    println!("cargo:rerun-if-env-changed=HOPOS_APPLE_DOCK");
    let out = PathBuf::from(env::var("OUT_DIR")?);
    let dst = out.join("embed.elf");
    match env::var("HOPOS_EMBED") {
        Ok(p) if !p.is_empty() => {
            println!("cargo:rerun-if-changed={p}");
            let bytes = fs::read(&p).map_err(|e| format!("HOPOS_EMBED {p}: {e}"))?;
            fs::write(&dst, bytes)?;
        }
        _ => fs::write(&dst, [])?,
    }
    let dock = match env::var("HOPOS_APPLE_DOCK") {
        Ok(v) if !v.trim().is_empty() => {
            let h = v.trim().trim_start_matches("0x").replace('_', "");
            let a = u64::from_str_radix(&h, 16)
                .map_err(|e| format!("HOPOS_APPLE_DOCK={v}: not a hex address ({e})"))?;
            if a == 0 || a >= 512 << 30 || a % 0x1000 != 0 {
                return Err(format!(
                    "HOPOS_APPLE_DOCK={v}: MMIO lives page-aligned under 512 GB (mmu.rs)"
                )
                .into());
            }
            format!("Some({a:#x})")
        }
        _ => "None".to_string(),
    };
    fs::write(
        out.join("dock.rs"),
        format!(
            "/// `HOPOS_APPLE_DOCK` bij het bouwen.\npub(crate) const DOCK_BUILD: Option<u64> = {dock};\n"
        ),
    )?;
    Ok(())
}
