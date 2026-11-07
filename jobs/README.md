# Jobspecs

De jobspecs van de apps die op de nodes draaien, overgenomen uit de wortel
van de Go-boom (in git stond daar alleen `job-lumen.json`; de rest droeg
geheimen en was genegeerd). Een spec gaat naar de leader van Hop:

```sh
cp jobs/job-stulp.json jobs/job-stulp.local.json   # de geheimen invullen
curl -X POST -d @jobs/job-stulp.local.json http://NODE:9080/v1/jobs
```

| Spec | Wat | Artifact |
| --- | --- | --- |
| `job-cloudflared.json` | cloudflared's eigen `tunnel run` als slot-app (30 MB, 256 MB partitie) | release `apps` van xinix00/HopOS, Go (`-tamago.elf`) |
| `job-cloudflared-lean.json` | het tunnelprotocol zelf op lean, 4 MB; wijst naar welcome op `10.100.0.2:80` | release van xinix00/hop (`apps/cloudflared-lean`, arm64 en riscv64) |
| `job-spin.json` | de Spin-server met zijn volume `/spin` | release `rolling` van xinix00/Spin |
| `job-stulp.json` | Stulp, de huisautomatisering, poort 80 en de attach-poort 7000 | release `apps` van xinix00/stulp |
| `job-stulp-plugins.json` | de plugins van Stulp (Matter op 5540), hangen aan Stulp | release `apps` van xinix00/stulp |
| `job-lumen.json` | Lumen, de media-app op de O6N, met de codec-firmware op `/firmware` | een lokale server, `http://192.168.1.208:8002` |
| `job-webdav.json` | de WebDAV-server op de media-volumes | een lokale server, `http://10.100.0.1:8088` |
| `hopos-media-o6n.cfg` | de config van de media-node: Lumen als init-job | (geen spec; `CFG=jobs/hopos-media-o6n.cfg MEDIA=1 BOARD=o6n sh image/uefi-run.sh`) |

De inhoud is die van de Go-generatie, op de geheimen na: `TUNNEL_TOKEN`,
`SPIN_MASTER_KEY`, `SPIN_WORKER_TOKEN`, `STULP_TOKEN` en
`STULP_ATTACH_SECRET` staan hier als `<NAAM>`, want deze boom is publiek.
Vul ze in een kopie in; `jobs/*.local.json` negeert git.

De specs van Lumen en WebDAV wijzen naar een server op het eigen LAN (de
Mac, of de node zelf op `10.100.0.1`), niet naar een release: zet het ELF
daar neer voor je de spec stuurt. Lumen en de media-config pinnen de node
op `hopos-1` (`affinity`, `hopos.node`).

## Het naamschema van de releases

Elke app van deze boom staat in de rollende release `apps` van
xinix00/HopOS als `<app>-<arch>.elf`, met `<arch>` de `node.arch` van Hop
(`arm64`, `riscv64`):

```
https://github.com/xinix00/HopOS/releases/download/apps/welcome-arm64.elf
```

`tools/release.sh` bouwt ze (de lijst `APPS` in `tools/lib.sh` plus decode
voor arm64, `APPS_RISCV64` ook voor riscv64) en zegt hoe ze in `apps` komen; de URL verschuift dus niet
per versie, en de headfull-config (`image/cfg/headfull.cfg`) start welcome
van daar.
De Go-apps (`go/apps-release.sh`, [go-apps.md](../docs/go-apps.md)) staan
in dezelfde release als `<app>-<arch>-tamago.elf`; `job-cloudflared.json`
wijst naar die vorm.
