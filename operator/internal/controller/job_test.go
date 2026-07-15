package controller

import (
	"strings"
	"testing"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"

	wavev1 "github.com/fastverk/wave/operator/api/v1"
)

// aionWeb is the driving case: bring aion/web's own @aion/* pins up to latest.
func aionWeb() *wavev1.WaveCascade {
	return &wavev1.WaveCascade{
		ObjectMeta: metav1.ObjectMeta{Name: "aion-web-internal-bump", Namespace: "fastverk"},
		Spec: wavev1.WaveCascadeSpec{
			Objective: "aion/web internal deps → latest",
			Forge:     "gitlab",
			Host:      "gitlab.savvifi.com",
			Scope: wavev1.WaveScope{
				Group:            "aion",
				Repos:            []string{"web"},
				InternalPrefixes: []string{"@aion/"},
			},
			Policy: wavev1.WavePolicy{IncludeInternal: true, Force: true, Autonomy: "Gated"},
			NpmScopeRegistries: []wavev1.NpmScopeRegistry{{
				Scope:    "@aion/",
				Registry: "https://gitlab.savvifi.com/api/v4/groups/195/-/packages/npm",
			}},
			Branch: "wave/dep-bumps",
		},
	}
}

func joined(wc *wavev1.WaveCascade) string {
	return strings.Join(waveArgs(wc), " ")
}

func TestWaveArgs_AionWebGated(t *testing.T) {
	got := joined(aionWeb())
	want := "discover --forge gitlab --host gitlab.savvifi.com --group aion --repos web " +
		"--internal-prefix @aion/ --include-internal --force " +
		"--npm-scope-registry @aion/=https://gitlab.savvifi.com/api/v4/groups/195/-/packages/npm " +
		"--open --open-branch wave/dep-bumps"
	if got != want {
		t.Fatalf("argv mismatch\n got: %s\nwant: %s", got, want)
	}
	// Gated must NOT arm auto-merge — that's the whole distinction.
	if strings.Contains(got, "--auto-merge") {
		t.Fatal("Gated autonomy must not pass --auto-merge")
	}
}

func TestWaveArgs_OpenArmsAutoMerge(t *testing.T) {
	wc := aionWeb()
	wc.Spec.Policy.Autonomy = "Open"
	if !strings.Contains(joined(wc), "--auto-merge") {
		t.Fatal("Open autonomy must arm auto-merge")
	}
}

func TestWaveArgs_DryRunOpensNothing(t *testing.T) {
	wc := aionWeb()
	wc.Spec.Policy.DryRun = true
	got := joined(wc)
	if strings.Contains(got, "--open") || strings.Contains(got, "--auto-merge") {
		t.Fatalf("dryRun must never write: %s", got)
	}
	if !strings.Contains(got, "--json") {
		t.Fatalf("dryRun should emit the machine-readable plan: %s", got)
	}
}

// DryRun beats Autonomy=Open: "report only" is the stronger statement, and the
// reverse would make a typo publish changes.
func TestWaveArgs_DryRunBeatsOpenAutonomy(t *testing.T) {
	wc := aionWeb()
	wc.Spec.Policy.DryRun = true
	wc.Spec.Policy.Autonomy = "Open"
	got := joined(wc)
	if strings.Contains(got, "--open") || strings.Contains(got, "--auto-merge") {
		t.Fatalf("dryRun must win over Autonomy=Open: %s", got)
	}
}

func TestWaveArgs_DefaultsAndOmissions(t *testing.T) {
	wc := &wavev1.WaveCascade{
		ObjectMeta: metav1.ObjectMeta{Name: "minimal", Namespace: "fastverk"},
		Spec: wavev1.WaveCascadeSpec{
			Objective: "minimal",
			Scope:     wavev1.WaveScope{Group: "fastverk"},
		},
	}
	got := joined(wc)
	// Forge defaults to gitlab; empty host/repos/prefixes contribute no flags.
	if !strings.HasPrefix(got, "discover --forge gitlab --group fastverk") {
		t.Fatalf("unexpected minimal argv: %s", got)
	}
	for _, flag := range []string{"--host", "--repos", "--internal-prefix", "--include-internal", "--force"} {
		if strings.Contains(got, flag) {
			t.Fatalf("%s must be omitted when unset: %s", flag, got)
		}
	}
}

func TestWaveArgs_MultiplePrefixesAndRegistries(t *testing.T) {
	wc := aionWeb()
	wc.Spec.Scope.InternalPrefixes = []string{"@aion/", "@savvi-studio/"}
	wc.Spec.NpmScopeRegistries = append(wc.Spec.NpmScopeRegistries, wavev1.NpmScopeRegistry{
		Scope:    "@savvi-studio/",
		Registry: "https://gitlab.savvifi.com/api/v4/groups/214/-/packages/npm",
	})
	got := joined(wc)
	if strings.Count(got, "--internal-prefix") != 2 {
		t.Fatalf("both prefixes must be passed: %s", got)
	}
	if strings.Count(got, "--npm-scope-registry") != 2 {
		t.Fatalf("both registries must be passed: %s", got)
	}
}

func TestRunJobName_StablePerTriggerAndDnsSafe(t *testing.T) {
	a1 := runJobName("aion-web-internal-bump", "trigger-1")
	a2 := runJobName("aion-web-internal-bump", "trigger-1")
	b := runJobName("aion-web-internal-bump", "trigger-2")
	if a1 != a2 {
		t.Fatal("the same trigger must address the same Job — that's the idempotency guard")
	}
	if a1 == b {
		t.Fatal("a new trigger must address a new Job")
	}
	if len(a1) > 63 {
		t.Fatalf("name exceeds the DNS label limit: %s", a1)
	}
}

func TestTruncateName_LongCascadeStillDnsSafeAndDistinct(t *testing.T) {
	long := strings.Repeat("a", 80)
	n1 := runJobName(long, "t1")
	n2 := runJobName(long, "t2")
	if len(n1) > 63 || len(n2) > 63 {
		t.Fatalf("truncation must respect 63 chars: %d/%d", len(n1), len(n2))
	}
	if n1 == n2 {
		t.Fatal("truncation must not collapse distinct triggers into one name")
	}
}
