package v1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// WaveScope selects what a cascade operates on.
type WaveScope struct {
	// Repos are the repo names (within Group) to act on, e.g. ["web"]. Empty ⇒
	// enumerate the whole group.
	// +optional
	Repos []string `json:"repos,omitempty"`
	// Group is the GitHub org / GitLab group path the repos live under (e.g.
	// "aion"). Required: it addresses the forge API even when Repos is explicit.
	// +kubebuilder:validation:MinLength=1
	Group string `json:"group"`
	// InternalPrefixes are the module-name prefixes owned in-house (e.g.
	// "@aion/"). By default these are SKIPPED — they belong to the cascade. Set
	// IncludeInternal to bump them instead.
	// +optional
	InternalPrefixes []string `json:"internalPrefixes,omitempty"`
}

// WavePolicy is the bump + merge posture.
type WavePolicy struct {
	// IncludeInternal bumps the InternalPrefixes modules too — "bring this repo's
	// own first-party pins up to latest". A module published by one of the scanned
	// repos stays internal regardless. This ADDS them to the third-party set; use
	// OnlyInternal to select them instead.
	// +optional
	IncludeInternal bool `json:"includeInternal,omitempty"`
	// OnlyInternal restricts the change to the InternalPrefixes modules (implies
	// IncludeInternal). Usually what you want for a first-party bump: otherwise
	// every third-party dep that happens to have drifted rides along in the same
	// change — a different blast radius, and usually a different reviewer.
	// +optional
	OnlyInternal bool `json:"onlyInternal,omitempty"`
	// Force bumps even where a caret/range already admits the latest version.
	// Load-bearing for a catalog workspace: `^0.2.0` ADMITS 0.2.3, so without
	// this there is nothing to do and the lockfile floor never moves.
	// +optional
	Force bool `json:"force,omitempty"`
	// Autonomy: Gated (open the change and hold it for review — the default) |
	// Open (arm merge-when-pipeline-succeeds).
	// +kubebuilder:validation:Enum=Gated;Open
	// +kubebuilder:default=Gated
	// +optional
	Autonomy string `json:"autonomy,omitempty"`
	// DryRun reports candidates without opening anything (`wave discover` with no
	// --open). The honest first move for a new cascade.
	// +optional
	DryRun bool `json:"dryRun,omitempty"`
}

// NpmScopeRegistry routes one package scope at a private registry, mirroring
// .npmrc's own `@scope:registry=` model.
type NpmScopeRegistry struct {
	// Scope is the package-name prefix, e.g. "@aion/".
	// +kubebuilder:validation:MinLength=1
	Scope string `json:"scope"`
	// Registry is the base URL. GitLab's group npm endpoint
	// (…/api/v4/groups/<id>/-/packages/npm) is packument-compatible, so it needs
	// no bespoke datasource — the forge token authorizes it.
	// +kubebuilder:validation:MinLength=1
	Registry string `json:"registry"`
}

// WaveCascadeSpec is the desired state of a WaveCascade.
type WaveCascadeSpec struct {
	// Objective is the human name for this cascade (e.g. "aion/web internal deps
	// → latest"). Surfaced in the CR listing.
	// +kubebuilder:validation:MinLength=1
	Objective string `json:"objective"`
	// Forge: "github" | "gitlab".
	// +kubebuilder:validation:Enum=github;gitlab
	// +kubebuilder:default=gitlab
	// +optional
	Forge string `json:"forge,omitempty"`
	// Host is the forge instance (e.g. gitlab.savvifi.com). Empty ⇒ the forge default.
	// +optional
	Host string `json:"host,omitempty"`
	// Scope selects the repos + which modules count as internal.
	Scope WaveScope `json:"scope"`
	// Policy is the bump + merge posture.
	// +optional
	Policy WavePolicy `json:"policy,omitempty"`
	// NpmScopeRegistries route first-party scopes at their private registry.
	// Required in practice when Policy.IncludeInternal is set: the public
	// registry 404s every first-party package, and a lookup miss is "no info",
	// not an error — so the run would look clean while doing nothing.
	// +optional
	NpmScopeRegistries []NpmScopeRegistry `json:"npmScopeRegistries,omitempty"`
	// Branch is the branch `--open` writes to. Stable on purpose: create_branch
	// and open_change are both idempotent, so a re-run REFRESHES the same change
	// with newer versions rather than opening another one.
	// +kubebuilder:default="wave/dep-bumps"
	// +optional
	Branch string `json:"branch,omitempty"`
	// Schedule is the cron cadence. Set ⇒ the operator maintains a CronJob; unset
	// ⇒ on-demand only (annotate with fastverk.savvifi.com/wave-now).
	// +optional
	Schedule string `json:"schedule,omitempty"`
	// Paused stops new runs. In-flight Jobs are not killed — the cascade just
	// stops being fed.
	// +optional
	Paused bool `json:"paused,omitempty"`
	// Image overrides the wave image (else the operator env WAVE_IMAGE).
	// +optional
	Image string `json:"image,omitempty"`
}

// WaveCascadePhase is the high-level lifecycle of a WaveCascade.
// +kubebuilder:validation:Enum=Pending;Scheduled;Running;Succeeded;Failed;Paused
type WaveCascadePhase string

const (
	// CascadePhasePending — not yet scheduled or run.
	CascadePhasePending WaveCascadePhase = "Pending"
	// CascadePhaseScheduled — a CronJob is maintained; no run in flight.
	CascadePhaseScheduled WaveCascadePhase = "Scheduled"
	// CascadePhaseRunning — a run Job is in flight.
	CascadePhaseRunning WaveCascadePhase = "Running"
	// CascadePhaseSucceeded — the last run completed.
	CascadePhaseSucceeded WaveCascadePhase = "Succeeded"
	// CascadePhaseFailed — the last run failed.
	CascadePhaseFailed WaveCascadePhase = "Failed"
	// CascadePhasePaused — spec.paused.
	CascadePhasePaused WaveCascadePhase = "Paused"
)

// WaveCascadeStatus is the observed state of a WaveCascade.
type WaveCascadeStatus struct {
	// ObservedGeneration is the .metadata.generation this status reflects.
	// +optional
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`
	// Phase is the high-level lifecycle.
	// +optional
	Phase WaveCascadePhase `json:"phase,omitempty"`
	// LastTrigger is the wave-now annotation value already handled. The watermark
	// that makes an on-demand trigger idempotent — without it the same annotation
	// would spawn a Job every reconcile.
	// +optional
	LastTrigger string `json:"lastTrigger,omitempty"`
	// ActiveJob is the in-flight run Job, if any. Also the single-flight guard:
	// the WaveStore is a ReadWriteOnce volume with one writer, so a second
	// concurrent run would contend for it.
	// +optional
	ActiveJob string `json:"activeJob,omitempty"`
	// CronJob is the maintained schedule's CronJob, when Schedule is set.
	// +optional
	CronJob string `json:"cronJob,omitempty"`
	// LastRunTime is when the last run Job finished.
	// +optional
	LastRunTime *metav1.Time `json:"lastRunTime,omitempty"`
	// Message is the last human-readable detail (why Failed, why nothing ran, …).
	// +optional
	Message string `json:"message,omitempty"`
	// Conditions follow the standard k8s condition contract.
	// +optional
	// +patchMergeKey=type
	// +patchStrategy=merge
	// +listType=map
	// +listMapKey=type
	Conditions []metav1.Condition `json:"conditions,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=wc,categories=fastverk
// +kubebuilder:printcolumn:name="Objective",type=string,JSONPath=`.spec.objective`
// +kubebuilder:printcolumn:name="Schedule",type=string,JSONPath=`.spec.schedule`
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Active",type=string,JSONPath=`.status.activeJob`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// WaveCascade declares a named, repeatable dependency-bump run: which repos,
// which modules count as first-party, and whether to open the change or just
// report. It is driven on demand (annotate with fastverk.savvifi.com/wave-now)
// and/or periodically (spec.schedule).
//
// The controller hands the cron string to Kubernetes and never ticks a timer
// itself — the same delegation the RbeCluster probe uses, and why no operator in
// the fleet carries a cron library.
type WaveCascade struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   WaveCascadeSpec   `json:"spec,omitempty"`
	Status WaveCascadeStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// WaveCascadeList contains a list of WaveCascade.
type WaveCascadeList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []WaveCascade `json:"items"`
}

func init() {
	SchemeBuilder.Register(&WaveCascade{}, &WaveCascadeList{})
}
