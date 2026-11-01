# Bevestigde opslagbarrière voor apps

`applib::sys::Client::sync(path)` stuurt de additieve `OP_SYNC` (20) over de
gewone system-API. Een bestaande bestands- of directorynaam bepaalt de
gebruikelijke slot-, generatie- en mountcontrole. `off`, `n` en data zijn
nul/leeg; een antwoord bevat uitsluitend de bevestigde HopFS-boomgeneratie.
Na verwijderen van een rollback-journal gebruikt de aanroeper de oudermap.

De HopFS-eigenaar heeft calls van meerdere apps tegelijk op de schijf,
maar per slot één tegelijk en in de volgorde van binnenkomst (alleen
lezingen mogen naast elkaar, een lees verandert niets): de barrière begint
dus pas als elke eerdere call van die app van het device terug is.
Er loopt hoogstens één vastlegging tegelijk; schrijfs van andere apps
lopen ernaast en gaan met de volgende mee. Bij wijzigingen voert hij de
bestaande commit uit: dataflush, metadata in de andere plek (body vóór
kop), metadataflush. Pas daarna bevestigt hij. Zonder
wijzigingen doet hij nog steeds een deviceflush. Een fout bij flush of
metadatawrite is het ijzer dat faalt: de kern valt (`HOPOS_FATAL`) en de
watchdog reset de node (docs/sequence-hopos.md §13.6); de app krijgt dan geen
antwoord, en dus nooit een bevestiging die niet duurzaam is.

De client herhaalt een verloren bevestiging niet. Zonder antwoord kan de
barrière tóch op schijf staan: SQLite/Replica moet dan zijn eigen journal-
en herstelprotocol volgen. Een oude kernel weigert het nieuwe opnummer.
Deze API vervangt geen exclusieve database-eigenaar, persistent volume of
veilig parkeren van de SQLite-C-stack tijdens async I/O.

Hosttests gebruiken een devicecache waarvan alleen `flush` de duurzame kopie
bijwerkt. Ze toetsen koude remount na schrijven, overschrijven met dezelfde
maat, verkleinen en journalverwijdering; iedere foutfase van commit (de
schijf blijft bij de vorige generatie); generatie-/padweigeringen en geen
automatische retry.

De volledige proef is `python3 tools/qemu-test-sync.py`. Hij start uitsluitend
een tijdelijke QEMU-schijf, plaatst `apps/syncprobe` via Hop met een eigen
volume, schrijft en synct een journal en 8192 databasebytes, verwijdert het
journal met een directorybarrière en stopt QEMU met SIGKILL. De volgende boot
moet hetzelfde volume lezen, de bytes controleren en het journal missen.
De console en checks komen in `target/qemu-sync/`. Dit is een virtio-toets;
het vervangt geen power-cut-proef met de fysieke NVMe-controller.
