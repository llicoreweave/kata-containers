//go:build linux

package drivers

import "testing"

func TestH100NVSwitchEndpoint(t *testing.T) {
	for _, tc := range []struct {
		class, vendor, device string
		want                  bool
	}{
		{"0x068000", "0x10de", "0x22a3", true},
		{"0x060400", "0x10de", "0x22a3", false},
		{"0x060000", "0x10de", "0x22a3", false},
		{"0x068000", "0x15b3", "0x22a3", false},
		{"0x068000", "0x10de", "0x2330", false},
	} {
		if got := isH100NVSwitchEndpoint(tc.class, tc.vendor, tc.device); got != tc.want {
			t.Errorf("class=%s vendor=%s device=%s: got %v want %v", tc.class, tc.vendor, tc.device, got, tc.want)
		}
	}
}
