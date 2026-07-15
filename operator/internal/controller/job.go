package controller

import (
	"crypto/sha256"
	"fmt"
	"os"
	"strings"

	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"

	wavev1 "github.com/fastverk/wave/operator/api/v1"
)

const (
	// envWaveImage is the wave CLI image the run Jobs use, when the CR doesn't
	// pin one.
	envWaveImage = "WAVE_IMAGE"
	// envForgeTokenSecret / envForgeTokenKey locate the forge credential. It
	// needs `api` scope — opening a change is a write; `read_api` is not enough.
	envForgeTokenSecret = "WAVE_FORGE_TOKEN_SECRET"
	envForgeTokenKey    = "WAVE_FORGE_TOKEN_KEY"
	// envServiceAccount is the SA the run Jobs use.
	envServiceAccount = "WAVE_JOB_SERVICE_ACCOUNT"
	// envStateClaim is the PVC holding the durable WaveStore. Optional: without
	// it a run is stateless, which is fine for the flat bump path (it keeps no
	// cross-run state) but not for a cascade.
	envStateClaim = "WAVE_STATE_CLAIM"

	stateMountPath = "/var/lib/wave"
)

func getenvDefault(k, def string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return def
}

func labelsFor(name string) map[string]string {
	return map[string]string{
		"app.kubernetes.io/name":       "wave",
		"app.kubernetes.io/component":  "cascade",
		"fastverk.savvifi.com/cascade": name,
	}
}

// waveArgs renders the `wave discover …` argv a cascade means. Keeping this pure
// (CR in, argv out) is what makes the whole scheduling surface unit-testable
// without a cluster — the Job is just a carrier.
func waveArgs(wc *wavev1.WaveCascade) []string {
	args := []string{"discover"}
	args = append(args, "--forge", firstNonEmpty(wc.Spec.Forge, "gitlab"))
	if wc.Spec.Host != "" {
		args = append(args, "--host", wc.Spec.Host)
	}
	args = append(args, "--group", wc.Spec.Scope.Group)
	if len(wc.Spec.Scope.Repos) > 0 {
		args = append(args, "--repos", strings.Join(wc.Spec.Scope.Repos, ","))
	}
	for _, p := range wc.Spec.Scope.InternalPrefixes {
		args = append(args, "--internal-prefix", p)
	}
	// only-internal implies include-internal, so pass just the narrower flag —
	// passing both is redundant and reads as if they were independent.
	if wc.Spec.Policy.OnlyInternal {
		args = append(args, "--only-internal")
	} else if wc.Spec.Policy.IncludeInternal {
		args = append(args, "--include-internal")
	}
	if wc.Spec.Policy.Force {
		args = append(args, "--force")
	}
	for _, r := range wc.Spec.NpmScopeRegistries {
		args = append(args, "--npm-scope-registry", fmt.Sprintf("%s=%s", r.Scope, r.Registry))
	}
	// DryRun wins over everything: report only, open nothing.
	if !wc.Spec.Policy.DryRun {
		args = append(args, "--open")
		if b := wc.Spec.Branch; b != "" {
			args = append(args, "--open-branch", b)
		}
		// Gated (the default) opens the change and holds it. Only Open arms
		// merge-when-pipeline-succeeds.
		if wc.Spec.Policy.Autonomy == "Open" {
			args = append(args, "--auto-merge")
		}
	} else {
		args = append(args, "--json")
	}
	return args
}

func firstNonEmpty(vals ...string) string {
	for _, v := range vals {
		if v != "" {
			return v
		}
	}
	return ""
}

// wavePodSpec is the single source of truth for how a run executes — used by
// BOTH the CronJob (periodic) and the one-shot Job (on-demand), so the two can
// never drift.
func wavePodSpec(wc *wavev1.WaveCascade) (corev1.PodSpec, error) {
	image := firstNonEmpty(wc.Spec.Image, os.Getenv(envWaveImage))
	if image == "" {
		return corev1.PodSpec{}, fmt.Errorf("wave image unset (spec.image or %s)", envWaveImage)
	}
	secret := os.Getenv(envForgeTokenSecret)
	if secret == "" {
		return corev1.PodSpec{}, fmt.Errorf(
			"forge token unset (%s): wave cannot read the forge, and --open needs `api` scope",
			envForgeTokenSecret,
		)
	}

	env := []corev1.EnvVar{{
		// wave's forge_factory reads GITLAB_TOKEN / GITHUB_TOKEN, else FORGE_TOKEN.
		// FORGE_TOKEN covers both forges from one CR field.
		Name: "FORGE_TOKEN",
		ValueFrom: &corev1.EnvVarSource{
			SecretKeyRef: &corev1.SecretKeySelector{
				LocalObjectReference: corev1.LocalObjectReference{Name: secret},
				Key:                  getenvDefault(envForgeTokenKey, "token"),
			},
		},
	}}

	spec := corev1.PodSpec{
		RestartPolicy:      corev1.RestartPolicyNever,
		ServiceAccountName: getenvDefault(envServiceAccount, "wave"),
		Containers: []corev1.Container{{
			Name:  "wave",
			Image: image,
			Args:  waveArgs(wc),
			Env:   env,
		}},
	}

	// The WaveStore is ReadWriteOnce with a single writer, which is exactly why
	// the controller enforces single-flight (see the reconciler). Mount it only
	// when configured; the flat bump path keeps no cross-run state.
	if claim := os.Getenv(envStateClaim); claim != "" {
		spec.Volumes = []corev1.Volume{{
			Name: "state",
			VolumeSource: corev1.VolumeSource{
				PersistentVolumeClaim: &corev1.PersistentVolumeClaimVolumeSource{ClaimName: claim},
			},
		}}
		spec.Containers[0].VolumeMounts = []corev1.VolumeMount{{
			Name:      "state",
			MountPath: stateMountPath,
		}}
		spec.Containers[0].Env = append(spec.Containers[0].Env, corev1.EnvVar{
			Name:  "FASTVERK_STATE_DIR",
			Value: stateMountPath,
		})
	}
	return spec, nil
}

// runJobName is derived from the trigger, so re-reconciling the SAME trigger
// addresses the same Job — that plus the status watermark is what stops an
// annotation from spawning a Job on every pass.
func runJobName(cascade, trigger string) string {
	h := sha256.Sum256([]byte(trigger))
	return truncateName(fmt.Sprintf("wave-%s-%x", cascade, h[:4]))
}

func cronJobName(cascade string) string {
	return truncateName(fmt.Sprintf("wave-%s", cascade))
}

// truncateName keeps a generated name inside the 63-char DNS label limit while
// staying collision-resistant: the tail (which carries the trigger hash) is
// preserved, the middle is dropped.
func truncateName(name string) string {
	const max = 63
	if len(name) <= max {
		return name
	}
	h := sha256.Sum256([]byte(name))
	suffix := fmt.Sprintf("-%x", h[:4])
	return name[:max-len(suffix)] + suffix
}

// buildRunJob renders a one-shot run. TTL-reaped so a busy schedule doesn't
// accumulate Jobs; BackoffLimit 0 because a retry would re-open the same change
// and the next tick is the retry.
func buildRunJob(wc *wavev1.WaveCascade, trigger string) (*batchv1.Job, error) {
	pod, err := wavePodSpec(wc)
	if err != nil {
		return nil, err
	}
	backoff := int32(0)
	ttl := int32(3600)
	return &batchv1.Job{
		ObjectMeta: metav1.ObjectMeta{
			Name:      runJobName(wc.Name, trigger),
			Namespace: wc.Namespace,
			Labels:    labelsFor(wc.Name),
		},
		Spec: batchv1.JobSpec{
			BackoffLimit:            &backoff,
			TTLSecondsAfterFinished: &ttl,
			Template: corev1.PodTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{Labels: labelsFor(wc.Name)},
				Spec:       pod,
			},
		},
	}, nil
}

// buildCronJob renders the periodic schedule. ForbidConcurrent because the
// WaveStore has a single writer — and because two overlapping runs would race to
// push the same branch.
func buildCronJob(wc *wavev1.WaveCascade) (*batchv1.CronJob, error) {
	pod, err := wavePodSpec(wc)
	if err != nil {
		return nil, err
	}
	backoff := int32(0)
	ttl := int32(3600)
	successLimit := int32(3)
	failedLimit := int32(3)
	return &batchv1.CronJob{
		ObjectMeta: metav1.ObjectMeta{
			Name:      cronJobName(wc.Name),
			Namespace: wc.Namespace,
			Labels:    labelsFor(wc.Name),
		},
		Spec: batchv1.CronJobSpec{
			Schedule:                   wc.Spec.Schedule,
			ConcurrencyPolicy:          batchv1.ForbidConcurrent,
			SuccessfulJobsHistoryLimit: &successLimit,
			FailedJobsHistoryLimit:     &failedLimit,
			// Suspend rather than delete on pause: the schedule is spec, and a
			// paused cascade should resume where it was, not be re-derived.
			Suspend: &wc.Spec.Paused,
			JobTemplate: batchv1.JobTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{Labels: labelsFor(wc.Name)},
				Spec: batchv1.JobSpec{
					BackoffLimit:            &backoff,
					TTLSecondsAfterFinished: &ttl,
					Template: corev1.PodTemplateSpec{
						ObjectMeta: metav1.ObjectMeta{Labels: labelsFor(wc.Name)},
						Spec:       pod,
					},
				},
			},
		},
	}, nil
}
