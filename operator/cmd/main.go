// wave-operator — reconciles WaveCascade into the Jobs that run the `wave` CLI.
//
// Inert until a WaveCascade exists: it reconciles nothing on its own, matching
// the other fastverk operators' posture.
//
// Config comes from the environment (see internal/controller/job.go):
//
//	WAVE_IMAGE                 the wave CLI image the run Jobs use (required)
//	WAVE_FORGE_TOKEN_SECRET    secret holding the forge token (required; `api`
//	                           scope — opening a change is a write)
//	WAVE_FORGE_TOKEN_KEY       key within it (default "token")
//	WAVE_JOB_SERVICE_ACCOUNT   SA for the run Jobs (default "wave")
//	WAVE_STATE_CLAIM           PVC for the durable WaveStore (optional)
package main

import (
	"flag"
	"os"

	"k8s.io/apimachinery/pkg/runtime"
	utilruntime "k8s.io/apimachinery/pkg/util/runtime"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/healthz"
	"sigs.k8s.io/controller-runtime/pkg/log/zap"
	metricsserver "sigs.k8s.io/controller-runtime/pkg/metrics/server"

	wavev1 "github.com/fastverk/wave/operator/api/v1"
	"github.com/fastverk/wave/operator/internal/controller"
)

var (
	scheme   = runtime.NewScheme()
	setupLog = ctrl.Log.WithName("setup")
)

func init() {
	utilruntime.Must(clientgoscheme.AddToScheme(scheme))
	utilruntime.Must(wavev1.AddToScheme(scheme))
}

func main() {
	var metricsAddr, probeAddr string
	var enableLeaderElection bool
	flag.StringVar(&metricsAddr, "metrics-bind-address", ":8080", "metrics endpoint bind address")
	flag.StringVar(&probeAddr, "health-probe-bind-address", ":8081", "health probe bind address")
	flag.BoolVar(&enableLeaderElection, "leader-elect", false,
		"enable leader election, so only one manager is active")
	opts := zap.Options{Development: false}
	opts.BindFlags(flag.CommandLine)
	flag.Parse()

	ctrl.SetLogger(zap.New(zap.UseFlagOptions(&opts)))

	mgr, err := ctrl.NewManager(ctrl.GetConfigOrDie(), ctrl.Options{
		Scheme:                 scheme,
		Metrics:                metricsserver.Options{BindAddress: metricsAddr},
		HealthProbeBindAddress: probeAddr,
		LeaderElection:         enableLeaderElection,
		LeaderElectionID:       "wave-operator.fastverk.savvifi.com",
	})
	if err != nil {
		setupLog.Error(err, "unable to start manager")
		os.Exit(1)
	}

	if err := (&controller.WaveCascadeReconciler{
		Client: mgr.GetClient(),
		Scheme: mgr.GetScheme(),
	}).SetupWithManager(mgr); err != nil {
		setupLog.Error(err, "unable to create controller", "controller", "WaveCascade")
		os.Exit(1)
	}

	if err := mgr.AddHealthzCheck("healthz", healthz.Ping); err != nil {
		setupLog.Error(err, "unable to set up health check")
		os.Exit(1)
	}
	if err := mgr.AddReadyzCheck("readyz", healthz.Ping); err != nil {
		setupLog.Error(err, "unable to set up ready check")
		os.Exit(1)
	}

	setupLog.Info("starting wave-operator")
	if err := mgr.Start(ctrl.SetupSignalHandler()); err != nil {
		setupLog.Error(err, "problem running manager")
		os.Exit(1)
	}
}
