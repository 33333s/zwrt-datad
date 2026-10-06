package main

import (
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestWorkerVerifiedOutcome(t *testing.T) {
	for _, tc := range []struct {
		name, script string
		ok           bool
	}{
		{"success", `printf 'SUCCESS: session; verified\n' > "$1/result"`, true},
		{"missing_result", `exit 0`, false},
		{"false_success", `echo 'SUCCESS: session' > "$1/result"; exit 1`, false},
		{"rollback", `echo 'FAILED: previous installation restored' > "$1/result"; exit 1`, false},
		{"deadline", `trap 'echo "FAILED: previous installation restored" > "$1/result"; exit 1' TERM; while :; do sleep 0.01; done`, false},
	} {
		t.Run(tc.name, func(t *testing.T) {
			stage := t.TempDir()
			os.WriteFile(filepath.Join(stage, "deploy-transaction.sh"), []byte(tc.script), 0700)
			deadline := time.Second
			if tc.name == "deadline" {
				deadline = 100 * time.Millisecond
			}
			err := executeTransaction(stage, deadline)
			if (err == nil) != tc.ok {
				t.Fatalf("error=%v", err)
			}
			b, err := os.ReadFile(filepath.Join(stage, "worker-result.json"))
			if err != nil {
				t.Fatal(err)
			}
			var result outcome
			if err = json.Unmarshal(b, &result); err != nil {
				t.Fatal(err)
			}
			if result.Success != tc.ok {
				t.Fatalf("outcome=%+v", result)
			}
			if tc.name == "deadline" && !strings.Contains(result.Message, "restored") {
				t.Fatal("rollback result lost")
			}
		})
	}
}

func TestWorkerStartFailureDoesNotWait(t *testing.T) {
	stage := t.TempDir()
	if _, err := startDetached("/no/such/program", stage); err == nil {
		t.Fatal("expected exec failure")
	}
	stage = t.TempDir()
	script := filepath.Join(stage, "exit-now")
	os.WriteFile(script, []byte("#!/bin/sh\nexit 3\n"), 0700)
	cmd, err := startDetached(script, stage)
	if err != nil {
		t.Fatal(err)
	}
	started := time.Now()
	err = followWorker(cmd, stage)
	if err == nil || time.Since(started) > 3*time.Second {
		t.Fatalf("worker failure not reported promptly: %v", err)
	}
}

// A short-lived launcher starts a setsid worker and then exits without waiting.
// The worker must still finish its transaction, with its own log descriptors.
func TestDetachedWorkerSurvivesLauncher(t *testing.T) {
	if os.Getenv("INSTALLER_TEST_LAUNCHER") == "1" {
		cmd, err := startDetached(os.Getenv("INSTALLER_TEST_SCRIPT"), os.Getenv("INSTALLER_TEST_STAGE"))
		if err != nil {
			os.Exit(2)
		}
		_ = cmd.Process.Release()
		os.Exit(0)
	}
	stage := t.TempDir()
	script := filepath.Join(stage, "worker")
	os.WriteFile(script, []byte("#!/bin/sh\nsleep 0.2\nprintf done > \"$2/completed\"\n"), 0700)
	self, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	cmd := exec.Command(self, "-test.run=^TestDetachedWorkerSurvivesLauncher$")
	cmd.Env = append(os.Environ(), "INSTALLER_TEST_LAUNCHER=1", "INSTALLER_TEST_SCRIPT="+script, "INSTALLER_TEST_STAGE="+stage)
	if b, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("launcher: %s %v", b, err)
	}
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		if b, _ := os.ReadFile(filepath.Join(stage, "completed")); string(b) == "done" {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatal("detached worker did not survive its launcher")
}
