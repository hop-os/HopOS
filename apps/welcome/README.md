# welcome

De pagina die een node een gezicht geeft, en de eerste Rust-app van HopOS
v3. Eén HTTP-server (leanhttp over `applib::appnet`) op de poort uit
`ER_PORT_HTTP` (standaard 80):

```
GET /         de pagina, per verzoek opgebouwd (de getallen kloppen dus)
GET /health   200, "ok"
```

De pagina toont de bunny in tekst, de naam van de node (`ER_ATTR_NODE_ID`
van Hop), het slot, de uptime, het aantal verzoeken, de partitie en de heap
(uit de tellers van applib), en de `Host` waarmee de browser hem vond. Alles
zit in de binary: geen CDN, geen webfont, geen script. Een node zonder
internet ziet er net zo uit.

Markers op de console van de kern: `HOPOS_WELCOME_UP port=<n>` als de
listener staat, en om de honderd verzoeken `HOPOS_WELCOME_REQUESTS`.

## Bouwen

```sh
cargo build --release --target aarch64-unknown-none-softfloat -p welcome
rust-objcopy --strip-debug target/aarch64-unknown-none-softfloat/release/welcome welcome.elf
```

Zonder debug-info (de symbolen blijven: de plaatsing leest ze). Eén ELF voor
elk ARM64-board: canoniek gelinkt, de kern legt hem op de partitie van elk
slot.

## Morgen op een board

Er is nog geen `hop`-CLI: de jobspec gaat met `curl` naar de leader van Hop
(poort 9080 op het adres van de node). De bench-node draait met
`hopos.insecure=1` in `hopos.cfg`; met `hopos.apikey` vraagt de API een
`X-Hop-Auth`-handtekening die curl niet zelf zet.

1. Zet `welcome.elf` op een HTTP-server die de node kan bereiken, bijvoorbeeld
   op de laptop naast het board:

   ```sh
   python3 -m http.server 8000 --bind 0.0.0.0
   ```

2. Plaats hem (`NODE` is het adres uit `HOPOS_NET_UP`, `LAPTOP` dat van de
   laptop):

   ```sh
   curl -X POST -H 'Content-Type: application/json' \
     -d '{"name":"welcome","driver":"hop",
          "artifacts":[{"url":"http://LAPTOP:8000/welcome.elf"}],
          "memory_limit":33554432,"ports":{"http":80}}' \
     http://NODE:9080/v1/jobs
   ```

   Op de console: `HOP_JOB_PLACED slot=2`, dan `slot 2: 1 port(s) published
   tcp+udp on the uplink: :80 HOPOS_SLOT_PUBLISH` (de kern zet poort 80 van
   de node door naar het slot), en `slot 2: welcome: serving http on
   10.100.0.3:80 ... HOPOS_WELCOME_UP port=80`.

3. Open `http://NODE/` in de browser: een donkere pagina met de koperen
   bunny, "You have reached a HopOS node.", vier tegels (node, slot, uptime,
   verzoeken) en een tabel met wat de app van zichzelf weet. `http://NODE/health`
   geeft `ok`.

4. Weg: `curl -X DELETE http://NODE:9080/v1/jobs/welcome`. De kern trekt de
   poort in (`HOPOS_SLOT_UNPUBLISH`) en `http://NODE/` antwoordt niet meer.

Blijvend, zodat een verse node meteen iets laat zien: dezelfde JSON op één
regel achter `hopos.init[]=` in `hopos.cfg`.

Poort 80 is vrij omdat Hop zelf 8080 en 9080 heeft; een jobspec die een van
die twee vraagt, faalt luid (`port 8080 is taken by slot 1`,
`HOPOS_SLOT_PUBLISH_FAIL`).

## Op QEMU

```sh
sh tools/qemu-test-welcome.sh                      # de hele kring, groen of rood
WEBPORT=8081 ARTIFACT=welcome sh image/qemu-run.sh # met de hand
```

Het tweede commando drukt de artifact-URL en het `curl`-commando af; de
pagina staat daarna op `http://127.0.0.1:8081/`.

## Vorm

- `src/main.rs`: de start, de server (`applibhttp`: één acceptor met een
  park voor stille keep-alive-verbindingen en een vaste pool van vier
  werkers, elk voor één verzoek; handboek §2: geen taak per verbinding) en
  de routes via `leanhttp::Mux`.
- `src/page.rs`: de pagina, puur en op de host getoetst; schrijft in een
  vooraf gereserveerde buffer en groeit nooit.
- De verbinding zelf is `applib::tcp::TcpConn` (feature `http` van
  applib): `appnet::TcpStream` als leanhttp-verbinding, met de termijnen op
  het timerwiel.
