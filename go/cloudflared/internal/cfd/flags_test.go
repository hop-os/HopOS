package cfd_test

// Deze test importeert de gepinde cloudflared en toetst onze opdrachtregel
// tegen zijn ECHTE flag-boom. Dat is het enige dat een typefout of een
// upstream-hernoeming vangt vóórdat een node zich met een cryptische
// hulptekst in een restart-lus draait — de rest van dit pakket weet niks van
// cloudflared en test alleen onze eigen keuzes.
//
// Draait alleen na tools/prepare-cloudflared.sh (de replace in go.mod).

import (
	"reflect"
	"strings"
	"testing"

	"github.com/urfave/cli/v2"

	"github.com/cloudflare/cloudflared/cmd/cloudflared/tunnel"

	"github.com/hop-os/HopOS/apps/cloudflared/internal/cfd"
)

// find zoekt een (sub)commando op naam.
func find(cmds []*cli.Command, name string) *cli.Command {
	for _, c := range cmds {
		if c.Name == name {
			return c
		}
	}
	return nil
}

// hasFlag zegt of een commando deze flag kent, onder welke naam ook.
func hasFlag(c *cli.Command, name string) bool {
	for _, f := range c.Flags {
		for _, n := range f.Names() {
			if n == name {
				return true
			}
		}
	}
	return false
}

// flagsIn haalt de --flags uit een opdrachtregel (waardes overslaan).
func flagsIn(args []string) []string {
	var out []string
	for _, a := range args {
		if strings.HasPrefix(a, "--") {
			out = append(out, strings.TrimPrefix(a, "--"))
		}
	}
	return out
}

func TestGeneratedFlagsExistInPinnedCloudflared(t *testing.T) {
	tunnelCmd := find(tunnel.Commands(), "tunnel")
	if tunnelCmd == nil {
		t.Fatal("de gepinde cloudflared heeft geen 'tunnel'-commando meer")
	}
	runCmd := find(tunnelCmd.Subcommands, "run")
	if runCmd == nil {
		t.Fatal("de gepinde cloudflared heeft geen 'tunnel run'-subcommando meer")
	}

	// Quick tunnel: alle flags horen op `tunnel` te zitten.
	quick, err := cfd.Load(func(k string) string {
		return map[string]string{"HOPOS_HOST": "10.0.0.7"}[k]
	}, "10.100.0.5").Args()
	if err != nil {
		t.Fatalf("Args: %v", err)
	}
	if quick[1] != "tunnel" {
		t.Errorf("quick tunnel roept %q aan, wil 'tunnel'", quick[1])
	}
	for _, f := range flagsIn(quick) {
		if !hasFlag(tunnelCmd, f) {
			t.Errorf("cloudflared's 'tunnel' kent --%s niet (upstream hernoemd?)", f)
		}
	}

	// Named tunnel: --token hoort op `run`, de rest op `tunnel` (die staan vóór
	// het subcommando in de opdrachtregel).
	named, err := cfd.Load(func(k string) string {
		return map[string]string{"TUNNEL_TOKEN": "tok", "HOPOS_HOST": "10.0.0.7"}[k]
	}, "10.100.0.5").Args()
	if err != nil {
		t.Fatalf("Args: %v", err)
	}
	split := len(named)
	for i, a := range named {
		if a == "run" {
			split = i
			break
		}
	}
	if split == len(named) {
		t.Fatal("named tunnel bevat geen 'run'")
	}
	for _, f := range flagsIn(named[:split]) {
		if !hasFlag(tunnelCmd, f) {
			t.Errorf("cloudflared's 'tunnel' kent --%s niet", f)
		}
	}
	for _, f := range flagsIn(named[split:]) {
		if !hasFlag(runCmd, f) {
			t.Errorf("cloudflared's 'tunnel run' kent --%s niet", f)
		}
	}
}

func TestBridgedNamesAreRealCloudflaredEnvVars(t *testing.T) {
	// Onze allowlist moet cloudflared's eigen env-namen zijn, anders zetten we
	// variabelen die niemand leest.
	known := map[string]bool{}
	collect := func(c *cli.Command) {
		for _, f := range c.Flags {
			for _, e := range envVarsOf(f) {
				known[e] = true
			}
		}
	}
	tunnelCmd := find(tunnel.Commands(), "tunnel")
	if tunnelCmd == nil {
		t.Fatal("geen 'tunnel'-commando")
	}
	collect(tunnelCmd)
	for _, sub := range tunnelCmd.Subcommands {
		collect(sub)
	}

	for _, name := range cfd.Bridged {
		if !known[name] {
			t.Errorf("%s is geen env-naam van de gepinde cloudflared", name)
		}
	}
}

// envVarsOf haalt de EnvVars uit een flag. Via reflectie en niet via een
// type-switch op cli.StringFlag & co: cloudflared wikkelt élke flag in een
// altsrc-variant (config-bestand-ondersteuning), dus een switch op de
// cli-typen ziet er geen één. Reflectie vindt het promoted EnvVars-veld
// ongeacht de wikkel.
func envVarsOf(f cli.Flag) []string {
	v := reflect.ValueOf(f)
	if v.Kind() == reflect.Ptr {
		if v.IsNil() {
			return nil
		}
		v = v.Elem()
	}
	if v.Kind() != reflect.Struct {
		return nil
	}
	field := v.FieldByName("EnvVars")
	if !field.IsValid() || field.Type() != reflect.TypeOf([]string{}) {
		return nil
	}
	return field.Interface().([]string)
}
