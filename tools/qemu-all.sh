#!/bin/sh
# De hele QEMU-suite na elkaar: elke ring uit docs/README.md (de tabel
# "Toetsen") met zijn eigen console in $LOGS, per ring één regel (groen of
# ROOD, de duur en de laatste regel van de ring), en onderaan één
# eindregel. Rood is rood: één rode ring maakt de suite rood (exit 1).
#
# Elke ring kiest zijn host-poorten zelf, vrij van het OS (tools/lib.sh
# `ports`): twee runners, of een runner en een agent met een losse toets,
# botsen niet op een vaste poort. Twee runners in DEZELFDE werkboom delen
# wel target/ (de hop-kopie van tools/hop-build.sh, de flip-bundels): elk
# zijn eigen werkboom.
#
#   sh tools/qemu-all.sh                alle ringen
#   sh tools/qemu-all.sh hop flip-uefi  alleen deze (de namen hieronder)
#   LOGS=map sh tools/qemu-all.sh       de consoles daar (standaard een
#                                       nieuwe tijdelijke map, die blijft)
#   HOP_DIR=pad                         de hop-repo (standaard ../hop/hop)
set -u

DIR="$(cd "$(dirname "$0")/.." && pwd)"
LOGS="${LOGS:-$(mktemp -d -t hopos-qemu-all.XXXXXX)}"
mkdir -p "$LOGS"

# naam, dan de opdracht (met de env ervoor), vanuit de werkboom.
RINGS='
virt          sh tools/qemu-test.sh
virt-gui      GUI=1 sh tools/qemu-test.sh
virt-display  GUI=display sh tools/qemu-test.sh
media         sh tools/qemu-test-media.sh
fault         sh tools/qemu-test-fault.sh
hop           sh tools/qemu-test-hop.sh
hop-smp2      SMP=2 sh tools/qemu-test-hop.sh
hop-oscore1   SMP=2 OSCORE=1 sh tools/qemu-test-hop.sh
welcome       sh tools/qemu-test-welcome.sh
hop-restart   sh tools/qemu-test-hop-restart.sh
hop-fail      sh tools/qemu-test-hop-fail.sh
smp           sh tools/qemu-test-smp.sh
share         sh tools/qemu-test-share.sh
reclaim       sh tools/qemu-test-reclaim.sh
bench         sh tools/qemu-test-bench.sh
slowdisk      sh tools/qemu-test-slowdisk.sh
volumes       sh tools/qemu-test-volumes.sh
store         sh tools/qemu-test-store.sh
sync          python3 tools/qemu-test-sync.py
flip          sh tools/qemu-test-flip.sh
flip-mismatch MISMATCH=1 sh tools/qemu-test-flip.sh
flip-oscore   OSCORE=1 sh tools/qemu-test-flip.sh
flip-rpi4     BOARD=rpi4 sh tools/qemu-test-flip.sh
flip-uefi     BOARD=uefi sh tools/qemu-test-flip.sh
uefi          sh tools/qemu-uefi-test.sh
uefi-gui      GUI=1 sh tools/qemu-uefi-test.sh
uefi-vhe      FEATURES=vhe CPU=neoverse-n1 sh tools/qemu-uefi-test.sh
nvme          sh tools/qemu-test-nvme.sh
rpi4          sh tools/qemu-rpi4-test.sh
riscv         sh tools/qemu-riscv-test.sh
riscv-hop     sh tools/qemu-riscv-test-hop.sh
riscv-share   sh tools/qemu-riscv-test-share.sh
'

# Bestaat elke gevraagde naam?
for want in "$@"; do
	echo "$RINGS" | grep -q "^$want " || {
		echo "qemu-all: geen ring '$want'; de ringen: $(echo "$RINGS" | awk 'NF { printf "%s ", $1 }')" >&2
		exit 64
	}
done

echo "== qemu-all: de consoles in $LOGS"
T0=$(date +%s)
n=0
red=""
echo "$RINGS" | {
	while read -r name cmd; do
		[ -n "$name" ] || continue
		if [ $# -gt 0 ]; then
			case " $* " in *" $name "*) ;; *) continue ;; esac
		fi
		n=$((n + 1))
		t=$(date +%s)
		if (cd "$DIR" && eval "exec env $cmd") </dev/null >"$LOGS/$name.log" 2>&1; then
			verdict=groen
		else
			verdict=ROOD
			red="$red $name"
		fi
		last="$(grep -v '^[[:space:]]*$' "$LOGS/$name.log" | tail -1)"
		printf '%-5s %-14s %4d s  %s\n' "$verdict" "$name" "$(($(date +%s) - t))" "$last"
	done
	if [ -z "$red" ]; then
		echo "qemu-all groen: $n ringen in $(($(date +%s) - T0)) s"
	else
		echo "qemu-all ROOD:$red ($(echo "$red" | wc -w | tr -d ' ') van $n) in $(($(date +%s) - T0)) s; de consoles in $LOGS"
		exit 1
	fi
}
