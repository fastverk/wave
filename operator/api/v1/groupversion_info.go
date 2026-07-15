// Package v1 contains the fastverk.savvifi.com/v1 API types the wave operator
// owns (WaveCascade). The group is shared with the other fastverk operators —
// each owns a disjoint set of kinds within it.
// +kubebuilder:object:generate=true
// +groupName=fastverk.savvifi.com
package v1

import (
	"k8s.io/apimachinery/pkg/runtime/schema"
	"sigs.k8s.io/controller-runtime/pkg/scheme"
)

var (
	// GroupVersion is the group/version used to register these objects.
	GroupVersion = schema.GroupVersion{Group: "fastverk.savvifi.com", Version: "v1"}

	// SchemeBuilder registers the group/version's Go types with a Scheme.
	SchemeBuilder = &scheme.Builder{GroupVersion: GroupVersion}

	// AddToScheme adds the group/version's types to a Scheme.
	AddToScheme = SchemeBuilder.AddToScheme
)
