//go:build linux

// SPDX-License-Identifier: Apache-2.0

package virtcontainers

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

// Publish one complete cold-boot configuration. A failed write must not replace
// the previous config with truncated JSON or follow a stale config symlink.
func (fc *firecracker) writeConfig() error {
	if fc.fcConfig == nil {
		return fmt.Errorf("Firecracker configuration is not initialized")
	}
	data, err := json.MarshalIndent(fc.fcConfig, "", "\t")
	if err != nil {
		return fmt.Errorf("encoding Firecracker configuration: %w", err)
	}
	file, err := os.CreateTemp(filepath.Dir(fc.fcConfigPath), ".fcConfig-*")
	if err != nil {
		return err
	}
	defer os.Remove(file.Name())
	defer file.Close()
	if err := file.Chmod(0640); err != nil {
		return err
	}
	if _, err := file.Write(data); err != nil {
		return err
	}
	if err := file.Sync(); err != nil {
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	return os.Rename(file.Name(), fc.fcConfigPath)
}
