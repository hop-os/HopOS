#!/bin/sh
# apps-release.sh: bouwt en publiceert de Go-apps van HopOS (docs/go-apps.md).
# Dat is nu alleen cloudflared; een volgende Go-app komt in APPS hieronder.
#
#   go/apps-release.sh                  # bouwen en publiceren
#   PUBLISH=0 go/apps-release.sh        # alleen bouwen (gate), niets uploaden
#   METAL=v2.2.8 go/apps-release.sh     # tegen een andere tag van de Go-SDK
#   TAG=apps-beta go/apps-release.sh    # een beta-app-release
#
# Elke app-module pint de Go-SDK (github.com/xinix00/HopOS/metal/v2) zelf in
# zijn go.mod. Met METAL bouwt dit script ze allemaal tegen die ene tag; de
# go.mod en go.sum gaan daarna terug zoals ze waren.
set -e

DIR="$(cd "$(dirname "$0")" && pwd)"
TAMAGO="${TAMAGO:-$HOME/tamago-go/bin/go}"
PUBLISH="${PUBLISH:-1}"
STAMP="${STAMP:-$(date -u +%Y.%m.%d-%H%M)}"
# TAG is de rollende app-release in de HopOS-repo waar de configs naar wijzen.
TAG="${TAG:-apps}"
OUT="${OUT:-$DIR/../target/go-apps}"

# naam:modulemap:cmd-submap. Alleen arm64: op riscv64 linkte een Go-app op de
# fysieke partitie en dat is op deze kern nog niet bewezen (docs/go-apps.md).
APPS="cloudflared:$DIR/cloudflared:cloudflared-hopos"

mkdir -p "$OUT"
[ -x "$TAMAGO" ] || { echo "FOUT: tamago ontbreekt op $TAMAGO (zet TAMAGO)" >&2; exit 1; }
command -v gh >/dev/null || [ "$PUBLISH" != "1" ] || { echo "FOUT: gh ontbreekt" >&2; exit 1; }
# Mag dit token hier schrijven: de vraag per repo, niet `gh api user` (die gaf
# tijdens de GitHub-storing van 17-08 als enige 503).
if [ "$PUBLISH" = "1" ]; then
	perm="$(gh api repos/hop-os/HopOS --jq .permissions.push 2>/dev/null || true)"
	[ "$perm" = "true" ] || {
		echo "FOUT: dit gh-token mag niet schrijven in hop-os/HopOS (push=${perm:-onbekend})." >&2
		echo "      herstel: gh auth switch --user derekdhaas" >&2; exit 1; }
fi

# 1. cloudflared bouwt tegen een gepatchte kopie van zijn eigen module; zonder
#    build/cloudflared-patched faalt elk go-commando in die module. Idempotent.
echo ">> cloudflared klaarzetten" >&2
sh "$DIR/cloudflared/tools/prepare-cloudflared.sh" >/dev/null

MODS=""
restore() {
	for m in $MODS; do
		mv -f "$m/go.mod.appsbak" "$m/go.mod" 2>/dev/null || true
		mv -f "$m/go.sum.appsbak" "$m/go.sum" 2>/dev/null || true
	done
}
trap restore EXIT INT TERM

if [ -n "${METAL:-}" ]; then
	for entry in $APPS; do
		mod="$(echo "$entry" | cut -d: -f2)"
		cp "$mod/go.mod" "$mod/go.mod.appsbak"
		cp "$mod/go.sum" "$mod/go.sum.appsbak"
		MODS="$MODS $mod"
		( cd "$mod" && GOWORK=off go mod edit -require "github.com/xinix00/HopOS/metal/v2@$METAL" &&
			GOWORK=off GOFLAGS=-mod=mod GOTOOLCHAIN=local go mod tidy ) ||
			{ echo "FOUT: metal $METAL in $mod" >&2; exit 1; }
	done
fi

# 2. Bouwen. Canonieke app-link (docs/go-apps.md): één artifact draait in elk
#    slot, dus arm64 linkt op SlotBase(1)+0x10000.
echo "== Go-apps (metal ${METAL:-uit go.mod}) ==" >&2
ELFS=""
for entry in $APPS; do
	name="${entry%%:*}"; mod="$(echo "$entry" | cut -d: -f2)"; cmd="${entry##*:}"
	elf="$OUT/$name-arm64-tamago.elf"
	printf '   %-30s' "$(basename "$elf")" >&2
	( cd "$mod" && GOWORK=off GOTOOLCHAIN=local \
		GOOS=tamago GOOSPKG=github.com/usbarmory/tamago GOARCH=arm64 \
		"$TAMAGO" build -tags linkcpuinit -trimpath \
		-ldflags "-w -T 0x50010000 -R 0x1000 -X main.version=$STAMP" -o "$elf" "./cmd/$cmd" )
	echo "$(( $(wc -c < "$elf") / 1024 )) kB" >&2
	ELFS="$ELFS $elf"
done

# 3. Geen enkel gvisor-symbool: een oude metal levert een werkende app op met
#    de verkeerde netstack erin, en dat ziet niemand aan de buitenkant.
echo ">> netstack-controle (0 gvisor-symbolen verwacht)" >&2
for elf in $ELFS; do
	n="$("$TAMAGO" tool nm "$elf" 2>/dev/null | grep -c gvisor || true)"
	[ "$n" -eq 0 ] || { echo "FOUT: $(basename "$elf") linkt $n gvisor-symbolen" >&2; exit 1; }
done
echo "   alle $(echo $ELFS | wc -w | tr -d ' ') images schoon" >&2

[ "$PUBLISH" = "1" ] || { echo "KLAAR (PUBLISH=0, niets geüpload): $OUT" >&2; exit 0; }

# 4. Publiceren naar HopOS/$TAG, naast de Rust-apps van tools/release.sh.
PRE=""
case "$TAG" in *beta*) PRE="--prerelease" ;; esac
U="https://github.com/hop-os/HopOS/releases/download/$TAG"
NOTES="Ready-to-run HopOS app images, canonically linked, so one artifact runs in any slot. Drop a URL in a jobspec and the node streams it straight onto a partition.

The Go apps (\`*-tamago.elf\`) are built $STAMP with TamaGo against metal ${METAL:-as pinned in go.mod}, verified to link zero gVisor symbols.

**cloudflared** runs cloudflared's own \`tunnel run\` as a slot app: a node behind NAT, with no inbound port, reachable over Cloudflare Tunnel. It publishes no port (it dials out) and wants a ~256 MB partition. Without TUNNEL_TOKEN you get a quick tunnel whose trycloudflare URL shows up in \`hop logs cloudflared\`; without TUNNEL_URL it points at \`http://\$HOPOS_HOST\`, port 80 of the node. Config uses cloudflared's own env names (TUNNEL_TOKEN/TUNNEL_URL/TUNNEL_TRANSPORT_PROTOCOL/TUNNEL_LOGLEVEL, plus CFD_EXTRA_ARGS); the default protocol is http2.

\`\`\`json
{\"name\":\"cloudflared\",\"driver\":\"hop\",\"artifacts\":[
  {\"url\":\"$U/cloudflared-arm64-tamago.elf\",\"match\":{\"node.arch\":\"arm64\"}}],
 \"memory_limit\":268435456,
 \"env\":{\"TUNNEL_TOKEN\":\"...\"}}
\`\`\`"

echo ">> uploaden naar HopOS/$TAG" >&2
# shellcheck disable=SC2086 # woordsplitsing over de elf-lijst is de bedoeling
if gh release view "$TAG" --repo hop-os/HopOS >/dev/null 2>&1; then
	gh release upload "$TAG" --repo hop-os/HopOS --clobber $ELFS >/dev/null
	gh release edit "$TAG" --repo hop-os/HopOS --notes "$NOTES" >/dev/null
else
	gh release create "$TAG" --repo hop-os/HopOS --latest=false $PRE \
		--title "HopOS app images${PRE:+ (beta)}" --notes "$NOTES" $ELFS >/dev/null
fi

echo "KLAAR: $U/<app>-arm64-tamago.elf" >&2
