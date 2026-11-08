//go:build tamago

// cloudflared-hopos draait cloudflared's eigen tunnel-CLI als HopOS-slot-app:
// een node achter NAT, zonder inkomende poort, zonder kernel — en toch publiek
// bereikbaar. Dit is de voorbeeld-app die laat zien dat "gewoon een Go-programma
// in een slot" ook geldt voor programma's van iemand anders: alles behalve twee
// platform-fallbacks (zie ../../patch) bouwt ongewijzigd voor tamago.
//
// De app praat alleen naar buiten (de tunnel dialt uit), dus hij hoeft geen
// poort te publiceren. Config uit de jobspec-env, met cloudflared's eigen
// env-namen:
//
//	TUNNEL_TOKEN               named tunnel; ingress komt uit het dashboard
//	                           leeg = quick tunnel (gratis trycloudflare-URL)
//	TUNNEL_URL                 de lokale service; default http://$HOPOS_HOST
//	                           (= de gepubliceerde poort 80 van deze node)
//	TUNNEL_TRANSPORT_PROTOCOL  http2 (default) of quic
//	TUNNEL_LOGLEVEL            info (default)
//	CFD_EXTRA_ARGS             vrije extra flags, op spaties gesplitst
//
// Jobspec — quick tunnel die de welcome-pagina van deze node publiek maakt:
//
//	{"name":"cloudflared","driver":"hop","artifacts":[
//	  {"url":"…/cloudflared-arm64-tamago.elf","match":{"node.arch":"arm64"}},
//	  {"url":"…/cloudflared-riscv64-tamago.elf","match":{"node.arch":"riscv64"}}],
//	 "memory_limit":268435456}
//
// De URL waarop de node dan te bereiken is, staat in `hop logs cloudflared`.
package main

import (
	"log"
	"os"
	"runtime"
	"runtime/debug"
	"strings"

	"github.com/urfave/cli/v2"

	// Zonder systeem-trust-store (er is geen /etc/ssl op een slot) valt
	// x509.SystemCertPool leeg terug, en cloudflared verifieert de edge daarmee.
	// Deze import zet de meegebakken rootbundel als fallback — zelfde bundel
	// die de apploader gebruikt om artifacts van https te halen.
	_ "golang.org/x/crypto/x509roots/fallback"

	"github.com/xinix00/HopOS/metal/v2/app/applib"
	"github.com/xinix00/HopOS/metal/v2/app/applib/appnet"

	"github.com/cloudflare/cloudflared/cmd/cloudflared/cliutil"
	"github.com/cloudflare/cloudflared/cmd/cloudflared/management"
	"github.com/cloudflare/cloudflared/cmd/cloudflared/tunnel"
	"github.com/cloudflare/cloudflared/cmd/cloudflared/updater"
	"github.com/cloudflare/cloudflared/metrics"
	"github.com/cloudflare/cloudflared/token"
	"github.com/cloudflare/cloudflared/tracing"

	"github.com/hop-os/HopOS/apps/cloudflared/internal/cfd"
)

var version = "dev" // -ldflags "-X main.version=vX.Y.Z"

// ringWriter stuurt stdlib-log naar de hop-ABI-logring (zelfde patroon als de
// satellieten). cloudflared zelf logt met zerolog naar stderr; dat vangt de
// pipe hieronder op.
type ringWriter struct{ app *applib.App }

func (w ringWriter) Write(p []byte) (int, error) {
	w.app.Logf("%s", strings.TrimRight(string(p), "\n"))
	return len(p), nil
}

func main() {
	app := applib.Init() // eerste regel: READY + heartbeat + kill-flag
	log.SetFlags(0)      // de ring stempelt zelf
	log.SetOutput(ringWriter{app: app})

	// Een panic in de main-goroutine als één leesbare regel, met de stack erboven
	// en de samenvatting als LAATSTE — dat is de regel die HOP bij het afsluiten
	// op de node-console echoot (last="…"). Runtime-output zelf komt sinds
	// HopOS' printk-sink óók in dit task-log terecht (applib hangt hem bij Init
	// in appboard), dus dit is geen nooduitgang meer maar alleen nettere vorm:
	// de reden vóór de stack in plaats van een afgekapte frame-regel.
	defer func() {
		if r := recover(); r != nil {
			for _, line := range strings.Split(strings.TrimRight(string(debug.Stack()), "\n"), "\n") {
				app.Logf("  %s", strings.ReplaceAll(line, "\t", "    "))
			}
			app.Logf("cloudflared: PANIC: %v", r)
			app.Exit(2)
		}
	}()

	ip, err := appnet.Up(app) // eigen TCP/IP-stack, eigen IP
	if err != nil {
		app.Logf("cloudflared: net: %v", err)
		app.Exit(1)
	}

	// Hier stond een os.Pipe die stdout/stderr naar de logring omleidde, omdat
	// cloudflared met zerolog naar os.Stderr schrijft en dat in een slot nergens
	// heen ging. Dat hoeft niet meer en het hóórt ook niet hier: HopOS levert de
	// bodem zelf. Een write naar fd 1/2 gaat via runtime.write1 → goos.Printk, en
	// board/hopslot geeft die door aan de sink die applib bij Init installeert —
	// oftewel het task-log van deze app. Dus alles wat cloudflared zegt, de
	// publieke URL van een quick tunnel incluis, staat gewoon in
	// `hop logs cloudflared`, zonder dat deze app iets omlegt.
	// (Op tamago bestond os.Pipe trouwens toch niet: "pipe: not implemented".)

	cfg := cfd.Load(app.Env, ip)
	args, err := cfg.Args()
	if err != nil {
		app.Logf("cloudflared: %v", err)
		app.Exit(1)
	}
	if bridged := cfd.Bridge(app.Env, os.Setenv); len(bridged) > 0 {
		app.Logf("cloudflared: env from job spec: %s", strings.Join(bridged, " "))
	}

	app.Logf("%s", cfg.Banner(version, ip))
	app.Logf("cloudflared: slot %d, %d core(s), %s RAM, %s %s",
		app.Slot, runtime.NumCPU(), fmtMB(app.RAMSize), runtime.GOARCH, runtime.Version())

	// cloudflared's eigen main doet méér dan commando's registreren: het zet een
	// handvol package-globals die het tunnel-pad daarna dereferenceert. Zonder
	// tunnel.Init is buildInfo nil en paniekt StartServer in
	// cliutil.(*BuildInfo).Log — in een slot volstrekt stil (board/hopslot's
	// printk is een no-op), dus HOP zag alleen "exit code 2". Gemeten in QEMU
	// 31-07; de host verried het niet, want daar ketste een ongeldige token af
	// vóór StartServer.
	//
	// Dit is precies de reeks uit cmd/cloudflared/main.go, beperkt tot de
	// pakketten die het tunnel-pad aanraakt (updater/token/management zitten er
	// in de code van `tunnel run`; access/tail horen bij andere commando's).
	bInfo := cliutil.GetBuildInfo("HopOS", version)
	graceShutdownC := make(chan struct{}) // HOP's kill is abrupt: dit sluit nooit
	tunnel.Init(bInfo, graceShutdownC)
	updater.Init(bInfo)
	management.Init(bInfo)
	token.Init(version)
	tracing.Init(version)
	metrics.RegisterBuildInfo("HopOS", "", version)

	// cloudflared's eigen TUN-8148-workaround, die hun main ook onvoorwaardelijk
	// zet: ECN-detectie door een eigen netstack is precies het soort ding dat je
	// hier niet wilt. (Het adres van de metrics-server zet cfd.Args expliciet —
	// zie DefaultMetricsPort voor waarom dat moet.)
	os.Setenv("QUIC_GO_DISABLE_ECN", "1")

	// De twee uitgangen van urfave/cli zijn variabelen, en dat is hier goud: een
	// fatale fout gaat via ErrWriter (default os.Stderr = het zwarte gat van een
	// slot) en daarna via OsExiter rechtstreeks naar os.Exit — zónder ooit terug
	// te keren uit Run. Zonder deze twee regels zag HOP alleen "exit code 1" en
	// bleef de reden onvindbaar.
	cli.ErrWriter = ringWriter{app: app}
	cli.OsExiter = func(code int) {
		app.Logf("cloudflared: exiting with code %d", code)
		app.Exit(uint64(uint32(code)))
	}

	// cloudflared's CLI leest os.Args, precies zoals op een gewone machine:
	// dezelfde subcommando's, dezelfde flags, dezelfde documentatie.
	os.Args = args
	cliApp := &cli.App{Name: "cloudflared", Version: version, Commands: tunnel.Commands()}
	app.Logf("cloudflared: exited: %v", cliApp.Run(os.Args))
	app.Exit(1) // een tunnel die stopt is een crash, by design: HOP herstart hem
}

// fmtMB houdt de logregel leesbaar zonder een formatter-pakket te linken.
func fmtMB(n uint64) string {
	const mb = 1 << 20
	if n < mb {
		return "<1 MB"
	}
	return strings.Join([]string{itoa(n / mb), "MB"}, " ")
}

func itoa(n uint64) string {
	if n == 0 {
		return "0"
	}
	var b [20]byte
	i := len(b)
	for n > 0 {
		i--
		b[i] = byte('0' + n%10)
		n /= 10
	}
	return string(b[i:])
}
