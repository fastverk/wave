// Package controller reconciles WaveCascade into the Jobs that run `wave`.
//
// The shape fuses two existing fastverk patterns, because no single CRD in the
// fleet does both:
//   - ReadinessCampaign (agents operator) — a config CR that owns child work and
//     mirrors its status back.
//   - RbeCluster.Spec.Probe (platform operator) — the only cron field in the
//     fleet: ONE shared pod spec feeding a k8s CronJob for the periodic path and
//     a trigger-annotation for the on-demand one, with a status watermark for
//     idempotency. The cron string is handed to Kubernetes; the controller never
//     ticks a timer (no operator in the fleet carries a cron library).
package controller

import (
	"context"
	"fmt"
	"time"

	batchv1 "k8s.io/api/batch/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
	"sigs.k8s.io/controller-runtime/pkg/log"

	wavev1 "github.com/fastverk/wave/operator/api/v1"
)

// waveNowAnnotation carries an opaque on-demand trigger token. Any new value
// starts one run; the handled value is watermarked in status.lastTrigger.
const waveNowAnnotation = "fastverk.savvifi.com/wave-now"

// requeueRunning is how often an in-flight run is re-checked.
const requeueRunning = 20 * time.Second

// WaveCascadeReconciler reconciles a WaveCascade object.
type WaveCascadeReconciler struct {
	client.Client
	Scheme *runtime.Scheme
}

// +kubebuilder:rbac:groups=fastverk.savvifi.com,resources=wavecascades,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=fastverk.savvifi.com,resources=wavecascades/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=fastverk.savvifi.com,resources=wavecascades/finalizers,verbs=update
// +kubebuilder:rbac:groups=batch,resources=jobs;cronjobs,verbs=get;list;watch;create;update;patch;delete

func (r *WaveCascadeReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	logger := log.FromContext(ctx)

	var wc wavev1.WaveCascade
	if err := r.Get(ctx, req.NamespacedName, &wc); err != nil {
		// Children are owner-ref'd, so deletion GCs them. Nothing to unwind.
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}

	// The schedule is reconciled even when paused — buildCronJob carries
	// Suspend=spec.paused, so pausing suspends rather than deletes. The schedule
	// is spec; a resumed cascade should pick its own cadence back up.
	if err := r.reconcileSchedule(ctx, &wc); err != nil {
		return ctrl.Result{}, err
	}

	// An in-flight run is the single-flight guard: the WaveStore is a RWO volume
	// with one writer, and two runs would also race to push the same branch.
	// Check before admitting anything new.
	running, err := r.reconcileActiveRun(ctx, &wc)
	if err != nil {
		return ctrl.Result{}, err
	}
	if running {
		return ctrl.Result{RequeueAfter: requeueRunning}, r.writeStatus(ctx, &wc, wavev1.CascadePhaseRunning, "run in flight")
	}

	if wc.Spec.Paused {
		return ctrl.Result{}, r.writeStatus(ctx, &wc, wavev1.CascadePhasePaused, "paused")
	}

	// On-demand: a wave-now value we haven't handled yet.
	started, err := r.reconcileOnDemand(ctx, &wc)
	if err != nil {
		return ctrl.Result{}, err
	}
	if started {
		logger.Info("started on-demand run", "job", wc.Status.ActiveJob)
		return ctrl.Result{RequeueAfter: requeueRunning}, r.writeStatus(ctx, &wc, wavev1.CascadePhaseRunning, "run started")
	}

	phase, msg := wavev1.CascadePhasePending, "awaiting a trigger"
	if wc.Spec.Schedule != "" {
		phase, msg = wavev1.CascadePhaseScheduled, fmt.Sprintf("scheduled %q", wc.Spec.Schedule)
	}
	if wc.Status.LastRunTime != nil {
		// Keep the last outcome visible rather than resetting to Pending — the
		// terminal phase of the last run is the useful thing to show between runs.
		if p := wc.Status.Phase; p == wavev1.CascadePhaseSucceeded || p == wavev1.CascadePhaseFailed {
			phase, msg = p, wc.Status.Message
		}
	}
	return ctrl.Result{}, r.writeStatus(ctx, &wc, phase, msg)
}

// reconcileSchedule creates/updates/removes the CronJob to match spec.schedule.
func (r *WaveCascadeReconciler) reconcileSchedule(ctx context.Context, wc *wavev1.WaveCascade) error {
	name := cronJobName(wc.Name)
	var existing batchv1.CronJob
	err := r.Get(ctx, types.NamespacedName{Namespace: wc.Namespace, Name: name}, &existing)

	// No schedule ⇒ on-demand only. Drop any CronJob a previous spec left behind.
	if wc.Spec.Schedule == "" {
		if err == nil {
			if delErr := r.Delete(ctx, &existing); delErr != nil && !apierrors.IsNotFound(delErr) {
				return fmt.Errorf("delete cronjob %s: %w", name, delErr)
			}
		}
		wc.Status.CronJob = ""
		return nil
	}

	desired, buildErr := buildCronJob(wc)
	if buildErr != nil {
		// A missing image/token is a config error, not a transient one. Surface it
		// on status rather than hot-looping on a retry that cannot succeed.
		wc.Status.Message = buildErr.Error()
		return nil
	}
	if ownErr := controllerutil.SetControllerReference(wc, desired, r.Scheme); ownErr != nil {
		return fmt.Errorf("set owner ref on cronjob: %w", ownErr)
	}

	switch {
	case apierrors.IsNotFound(err):
		if createErr := r.Create(ctx, desired); createErr != nil && !apierrors.IsAlreadyExists(createErr) {
			return fmt.Errorf("create cronjob %s: %w", name, createErr)
		}
	case err != nil:
		return fmt.Errorf("get cronjob %s: %w", name, err)
	default:
		// Spec-driven fields only — never clobber what the apiserver owns.
		existing.Spec.Schedule = desired.Spec.Schedule
		existing.Spec.Suspend = desired.Spec.Suspend
		existing.Spec.JobTemplate = desired.Spec.JobTemplate
		if updErr := r.Update(ctx, &existing); updErr != nil {
			return fmt.Errorf("update cronjob %s: %w", name, updErr)
		}
	}
	wc.Status.CronJob = name
	return nil
}

// reconcileActiveRun reports whether a run is still in flight, and folds a
// finished one into status. Returns true while running.
func (r *WaveCascadeReconciler) reconcileActiveRun(ctx context.Context, wc *wavev1.WaveCascade) (bool, error) {
	if wc.Status.ActiveJob == "" {
		return false, nil
	}
	var job batchv1.Job
	err := r.Get(ctx, types.NamespacedName{Namespace: wc.Namespace, Name: wc.Status.ActiveJob}, &job)
	switch {
	case apierrors.IsNotFound(err):
		// TTL-reaped before we observed it finish. Clearing rather than wedging is
		// the honest move: the run is definitively not in flight, and a stuck
		// ActiveJob would block every future run forever.
		wc.Status.ActiveJob = ""
		return false, nil
	case err != nil:
		return false, fmt.Errorf("get run job %s: %w", wc.Status.ActiveJob, err)
	}

	switch {
	case job.Status.Succeeded > 0:
		now := metav1.Now()
		wc.Status.ActiveJob = ""
		wc.Status.LastRunTime = &now
		wc.Status.Phase = wavev1.CascadePhaseSucceeded
		wc.Status.Message = "run completed"
		return false, nil
	case job.Status.Failed > 0:
		now := metav1.Now()
		wc.Status.ActiveJob = ""
		wc.Status.LastRunTime = &now
		wc.Status.Phase = wavev1.CascadePhaseFailed
		wc.Status.Message = "run failed — see the Job's logs"
		return false, nil
	}
	return true, nil
}

// reconcileOnDemand starts a run when the wave-now annotation carries a trigger
// not yet reflected in status.lastTrigger. Returns true if it started one.
func (r *WaveCascadeReconciler) reconcileOnDemand(ctx context.Context, wc *wavev1.WaveCascade) (bool, error) {
	trigger := wc.Annotations[waveNowAnnotation]
	if trigger == "" || trigger == wc.Status.LastTrigger {
		return false, nil
	}
	name := runJobName(wc.Name, trigger)

	var job batchv1.Job
	err := r.Get(ctx, types.NamespacedName{Namespace: wc.Namespace, Name: name}, &job)
	switch {
	case apierrors.IsNotFound(err):
		newJob, buildErr := buildRunJob(wc, trigger)
		if buildErr != nil {
			// Watermark the trigger even though nothing ran: the config is broken,
			// and re-deriving the same failure every pass is a hot loop, not
			// progress. The reason lands on status.
			wc.Status.LastTrigger = trigger
			wc.Status.Message = buildErr.Error()
			return false, nil
		}
		if ownErr := controllerutil.SetControllerReference(wc, newJob, r.Scheme); ownErr != nil {
			return false, fmt.Errorf("set owner ref on run job: %w", ownErr)
		}
		if createErr := r.Create(ctx, newJob); createErr != nil && !apierrors.IsAlreadyExists(createErr) {
			return false, fmt.Errorf("create run job %s: %w", name, createErr)
		}
	case err != nil:
		return false, fmt.Errorf("get run job %s: %w", name, err)
	}

	// Watermark + adopt in one pass, so a re-reconcile before the Job is observed
	// doesn't start a second one.
	wc.Status.LastTrigger = trigger
	wc.Status.ActiveJob = name
	return true, nil
}

func (r *WaveCascadeReconciler) writeStatus(
	ctx context.Context,
	wc *wavev1.WaveCascade,
	phase wavev1.WaveCascadePhase,
	msg string,
) error {
	wc.Status.Phase = phase
	wc.Status.Message = msg
	wc.Status.ObservedGeneration = wc.Generation
	if err := r.Status().Update(ctx, wc); err != nil {
		// A conflict means someone else advanced it; the next pass re-derives.
		return client.IgnoreNotFound(err)
	}
	return nil
}

// SetupWithManager wires the controller.
func (r *WaveCascadeReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&wavev1.WaveCascade{}).
		Owns(&batchv1.Job{}).
		Owns(&batchv1.CronJob{}).
		Complete(r)
}
