//! De platform-config: de tekst waarmee een operator een node instelt
//! zonder te herbouwen.
//!
//! Eén parser voor élk kanaal waarlangs die tekst binnenkomt (de
//! firmware-FAT, de initramfs-regio, het ingebakken blob, de
//! kernel-cmdline), omdat drie iets verschillende lezingen van hetzelfde
//! bestand precies de klasse fout opleveren die je bij boot niet ziet: de
//! node komt op, maar met een ándere config dan er staat.
//!
//! Twee formaten, en dat verschil is echt:
//!
//! - [`all`]: het CONFIGBESTAND (`hopos.cfg`). Eén `key=value` per regel,
//!   `#` vooraan is commentaar, en een waarde mag spaties bevatten.
//! - [`cmdline`]: de KERNEL-CMDLINE (/chosen/bootargs). Eén regel met
//!   whitespace-gescheiden tokens, dus een waarde heeft nooit een spatie.
//!
//! Waarom [`all`] niet op whitespace splitst: dan wordt
//! `# hopos.insecure=1` twee tokens, "#" en "hopos.insecure=1", en dat
//! tweede token matcht de sleutel gewoon. Een uitgecommentarieerde regel
//! MET spatie achter de # zette in de Go-kern zo de API open. Een
//! tekstformaat waarin een spatie een auth-poort opent is geen formaat om
//! op te vertrouwen.
//!
//! Een enkelvoudige sleutel die vaker staat: de LAATSTE waarde wint
//! ([`get`], 04-10). Zo is de config van een smaak (image/cfg) met een
//! eigen bestand erachter gewoon de aaneengeplakte tekst; een herhaalde
//! sleutel (`hopos.init[]`) telt beide.
//!
//! De node heeft twee bronnen, en hun volgorde staat hier één keer
//! ([`layered`], [`param`]): eerst de `hopos.*`-tokens van de bootargs, dan
//! het bestand. Het bestand wint dus van de bootargs (Go:
//! `rk3566.BootParam`), en een herhaalde sleutel telt beide.
//!
//! Geen allocatie: de waarden zijn slices van de tekst.

/// Alle waarden van `key` uit een configbestand, in bestandsvolgorde.
/// Enkelvoudige sleutels (`hopos.node`) hebben er één, herhaalde
/// (`hopos.init[]`) meerdere.
pub fn all<'a>(text: &'a str, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    text.split('\n').filter_map(move |line| {
        let line = line.strip_suffix('\r').unwrap_or(line).trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let v = line.strip_prefix(key)?.strip_prefix('=')?;
        Some(v.trim())
    })
}

/// Alle waarden van `key` uit een kernel-cmdline-regel. Niet-hopos-tokens
/// (Linux-restanten op de kaart) vallen vanzelf af.
pub fn cmdline<'a>(args: &'a str, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    args.split_ascii_whitespace()
        .filter_map(move |tok| tok.strip_prefix(key)?.strip_prefix('='))
}

/// De laatste waarde van `key` uit een configbestand, of "": de
/// enkelvoudige-sleutel-vorm van [`all`].
#[must_use]
pub fn get<'a>(text: &'a str, key: &'a str) -> &'a str {
    all(text, key).last().unwrap_or("")
}

/// De laatste waarde van `key` uit een kernel-cmdline-regel, of "": de
/// enkelvoudige-sleutel-vorm van [`cmdline`].
#[must_use]
pub fn get_cmdline<'a>(args: &'a str, key: &'a str) -> &'a str {
    cmdline(args, key).last().unwrap_or("")
}

/// Alle waarden van `key` uit de twee bronnen van een node: eerst de
/// bootargs `args`, dan het configbestand `file`.
pub fn layered<'a>(
    file: &'a str,
    args: &'a str,
    key: &'a str,
) -> impl Iterator<Item = &'a str> + 'a {
    cmdline(args, key).chain(all(file, key))
}

/// De laatste waarde van `key` uit [`layered`], of "": het bestand wint van
/// de bootargs.
#[must_use]
pub fn param<'a>(file: &'a str, args: &'a str, key: &'a str) -> &'a str {
    layered(file, args, key).last().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Het configbestand: één sleutel per regel, commentaar weg, waarden mét
    /// spaties intact.
    #[test]
    fn all_reads_the_file_format() {
        let text = "# HopOS node config\r\n\nhopos.node=hopos-lrv\nhopos.insecure=1\n\
            hopos.init[]={\"name\":\"welcome\",\"ports\":{\"http\":80}}\n\
            hopos.init[]={\"name\":\"tweede\", \"cmd\":\"met spaties\"}\n   hopos.console=5555\n";
        assert_eq!(all(text, "hopos.node").collect::<Vec<_>>(), ["hopos-lrv"]);
        let inits: Vec<_> = all(text, "hopos.init[]").collect();
        assert_eq!(inits.len(), 2);
        // Een waarde is de REST VAN DE REGEL.
        assert_eq!(inits[1], r#"{"name":"tweede", "cmd":"met spaties"}"#);
        // Ingesprongen regels werken; een bestand is geen assembly.
        assert_eq!(all(text, "hopos.console").collect::<Vec<_>>(), ["5555"]);
        assert_eq!(all(text, "hopos.cluster").count(), 0);
        // Een sleutel die een prefix van een andere is, matcht die niet.
        assert_eq!(all("hopos.nodes=x\n", "hopos.node").count(), 0);
    }

    /// DE REGRESSIE: een uitgecommentarieerde regel is commentaar, óók met
    /// een spatie achter de #.
    #[test]
    fn comment_with_a_space_is_not_config() {
        for text in [
            "# hopos.insecure=1\n",
            "#hopos.insecure=1\n",
            "#\thopos.insecure=1\n",
            "   # hopos.insecure=1\n",
        ] {
            assert_eq!(all(text, "hopos.insecure").count(), 0, "{text:?}");
        }
        assert_eq!(get("hopos.insecure=1\n", "hopos.insecure"), "1");
    }

    /// De cmdline: tokens op één regel, Linux-restanten negeren.
    #[test]
    fn cmdline_reads_tokens() {
        let args = "console=serial0,115200 root=/dev/mmcblk0p2 hopos.node=hop-1 \
            hopos.init[]={\"name\":\"a\"} hopos.init[]={\"name\":\"b\"}";
        assert_eq!(get_cmdline(args, "hopos.node"), "hop-1");
        assert_eq!(cmdline(args, "hopos.init[]").count(), 2);
        assert_eq!(cmdline(args, "hopos.cores").count(), 0);
    }

    #[test]
    fn get_of_nothing_is_empty_and_takes_the_last() {
        assert_eq!(get("", "hopos.node"), "");
        assert_eq!(get_cmdline("hopos.x=a hopos.x=b", "hopos.cores"), "");
        assert_eq!(get("hopos.x=a\nhopos.x=b\n", "hopos.x"), "b");
        assert_eq!(get_cmdline("hopos.x=a hopos.x=b", "hopos.x"), "b");
        // Lagen: een uitgecommentarieerde regel in een latere laag telt niet.
        assert_eq!(get("hopos.x=a\n# hopos.x=b\n", "hopos.x"), "a");
    }

    /// Het bestand wint van de bootargs; een herhaalde sleutel telt beide.
    #[test]
    fn the_file_wins_over_the_bootargs() {
        let file = "hopos.stage=app\nhopos.cores=2\nhopos.init[]=b\n";
        let args = "console=ttyAMA0 hopos.stage=hop hopos.oscore=big hopos.init[]=a";
        assert_eq!(param(file, args, "hopos.stage"), "app");
        assert_eq!(param(file, args, "hopos.oscore"), "big");
        assert_eq!(param("", args, "hopos.stage"), "hop");
        assert_eq!(param(file, "", "hopos.cores"), "2");
        assert_eq!(param(file, args, "hopos.node"), "");
        assert_eq!(
            layered(file, args, "hopos.init[]").collect::<Vec<_>>(),
            ["a", "b"]
        );
    }
}
