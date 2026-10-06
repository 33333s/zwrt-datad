package main

import (
	"context"
	"crypto/sha256"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"syscall"
	"time"
)

const installDir = "/cache/zwrt-datad"

type outcome struct {
	Success bool
	Message string
}

func main() {
	base := flag.String("base-url", release.BaseURL, "HTTPS directory containing zwrt-datad-armv7")
	local := flag.String("file", "", "use a local datad binary (offline installation)")
	preflight := flag.Bool("preflight", false, "check device without installing")
	verify := flag.String("verify-download", "", "download and verify to a NEW path, without installing")
	worker := flag.String("worker", "", "internal detached deployment worker")
	flag.Parse()
	var err error
	switch {
	case *worker != "":
		err = runWorker(*worker)
	case *verify != "":
		err = obtain(*base, *local, *verify)
	default:
		err = install(*base, *local, *preflight)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "ERROR:", err)
		os.Exit(1)
	}
}

func runCommand(timeout time.Duration, name string, args ...string) (string, error) {
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, name, args...)
	cmd.WaitDelay = 2 * time.Second
	b, err := cmd.CombinedOutput()
	if ctx.Err() != nil {
		return string(b), fmt.Errorf("%s timed out", name)
	}
	return string(b), err
}

func preflight() error {
	if os.Geteuid() != 0 {
		return fmt.Errorf("root ADB/shell is required")
	}
	if runtime.GOARCH != "arm" {
		return fmt.Errorf("installer requires ARMv7 Linux")
	}
	model, err := runCommand(5*time.Second, "cfg", "get", "model_name")
	if err != nil || strings.TrimSpace(model) != "MU5120" {
		return fmt.Errorf("device is not verified as U50 Pro / MU5120")
	}
	arch, err := runCommand(5*time.Second, "uname", "-m")
	if err != nil || strings.TrimSpace(arch) != "armv7l" {
		return fmt.Errorf("expected armv7l, got %q", arch)
	}
	for _, name := range []string{"sh", "sha256sum", "timeout", "awk", "grep", "readlink", "cp", "mv", "chmod", "mkdir", "rmdir", "rm", "ln", "cmp", "cat", "dirname", "mount", "sync", "sleep", "systemctl"} {
		if _, err := exec.LookPath(name); err != nil {
			return fmt.Errorf("required device utility is missing: %s", name)
		}
	}
	if info, err := os.Lstat(installDir); err == nil {
		if !info.IsDir() || info.Mode()&os.ModeSymlink != 0 {
			return fmt.Errorf("unsafe installation directory")
		}
	} else if !os.IsNotExist(err) {
		return err
	}
	if _, err := os.Lstat(installDir + "/.deploy-lock"); !os.IsNotExist(err) {
		return fmt.Errorf("deployment lock exists; inspect its PID/result before retrying")
	}
	var stat syscall.Statfs_t
	if err := syscall.Statfs("/cache", &stat); err != nil {
		return err
	}
	if stat.Bavail*uint64(stat.Bsize) < uint64(release.Bytes*3+(4<<20)) {
		return fmt.Errorf("insufficient free space on /cache")
	}
	return nil
}

func install(base, local string, checkOnly bool) error {
	fmt.Printf("U50 Pro installer r2 / datad %s\n[1/5] Checking device...\n", release.Version)
	if err := preflight(); err != nil {
		return err
	}
	if checkOnly {
		fmt.Println("Preflight OK (no files changed)")
		return nil
	}
	if err := os.MkdirAll(installDir, 0700); err != nil {
		return err
	}
	stage, err := os.MkdirTemp(installDir, ".deploy.")
	if err != nil {
		return err
	}
	fmt.Println("Work directory:", stage)
	fmt.Println("[2/5] Obtaining and verifying binary...")
	binary := filepath.Join(stage, "zwrt-datad")
	if err = obtain(base, local, binary); err != nil {
		return err
	}
	if err = os.Chmod(binary, 0700); err != nil {
		return err
	}
	version, err := runCommand(10*time.Second, binary, "--version")
	if err != nil || strings.TrimSpace(version) != "zwrt-datad "+release.Version {
		return fmt.Errorf("binary version check failed: %q (%v)", version, err)
	}
	fmt.Println("[3/5] Preparing checked deployment files...")
	if err = prepareStage(stage); err != nil {
		return err
	}
	self, err := os.Executable()
	if err != nil {
		return err
	}
	cmd, err := startDetached(self, stage)
	if err != nil {
		return err
	}
	fmt.Printf("[4/5] Installing; worker PID %d (survives ADB disconnect)...\n", cmd.Process.Pid)
	fmt.Println("Log:", filepath.Join(stage, "worker.log"))
	return followWorker(cmd, stage)
}

func prepareStage(stage string) error {
	files := map[string][]byte{"service-control.sh": serviceScript, "deploy-transaction.sh": transactionScript, "start.sh": startScript, "zwrt-datad.service": unitFile}
	for name, data := range files {
		if len(data) == 0 {
			return fmt.Errorf("installer was built without embedded %s", name)
		}
		if err := os.WriteFile(filepath.Join(stage, name), data, 0700); err != nil {
			return err
		}
	}
	var manifest strings.Builder
	for _, name := range []string{"zwrt-datad", "service-control.sh", "deploy-transaction.sh", "start.sh", "zwrt-datad.service"} {
		f, err := os.Open(filepath.Join(stage, name))
		if err != nil {
			return err
		}
		hash := sha256.New()
		_, err = io.Copy(hash, f)
		f.Close()
		if err != nil {
			return err
		}
		fmt.Fprintf(&manifest, "%x  %s\n", hash.Sum(nil), name)
	}
	return os.WriteFile(filepath.Join(stage, "SHA256SUMS"), []byte(manifest.String()), 0600)
}

func startDetached(self, stage string) (*exec.Cmd, error) {
	log, err := os.OpenFile(filepath.Join(stage, "worker.log"), os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if err != nil {
		return nil, err
	}
	defer log.Close()
	input, err := os.Open(os.DevNull)
	if err != nil {
		return nil, err
	}
	defer input.Close()
	cmd := exec.Command(self, "--worker", stage)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	cmd.Stdin = input
	cmd.Stdout = log
	cmd.Stderr = log
	if err = cmd.Start(); err != nil {
		return nil, err
	}
	return cmd, nil
}

func runWorker(stage string) error {
	if os.Geteuid() != 0 || filepath.Dir(stage) != installDir || !strings.HasPrefix(filepath.Base(stage), ".deploy.") {
		return fmt.Errorf("invalid worker context")
	}
	real, err := filepath.EvalSymlinks(stage)
	if err != nil || real != stage {
		return fmt.Errorf("unsafe staging path")
	}
	syscall.Umask(0077)
	signal.Ignore(syscall.SIGHUP)
	return executeTransaction(stage, 3*time.Minute)
}

func executeTransaction(stage string, deadline time.Duration) error {
	if err := os.WriteFile(filepath.Join(stage, "worker.pid"), []byte(strconv.Itoa(os.Getpid())+"\n"), 0600); err != nil {
		return err
	}
	fmt.Println("Deployment worker started; running transaction.")
	cmd := exec.Command("sh", filepath.Join(stage, "deploy-transaction.sh"), stage)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	err := cmd.Start()
	if err == nil {
		finished := make(chan error, 1)
		go func() { finished <- cmd.Wait() }()
		timer := time.NewTimer(deadline)
		defer timer.Stop()
		select {
		case err = <-finished:
		case <-timer.C:
			fmt.Println("Deployment deadline reached; requesting rollback. Waiting for recovery result...")
			_ = cmd.Process.Signal(syscall.SIGTERM)
			err = <-finished
			if err == nil {
				err = fmt.Errorf("deployment exceeded deadline")
			}
		}
	}
	return saveOutcome(stage, err)
}

func saveOutcome(stage string, processErr error) error {
	b, readErr := os.ReadFile(filepath.Join(stage, "result"))
	message := strings.TrimSpace(string(b))
	success := processErr == nil && readErr == nil && strings.HasPrefix(message, "SUCCESS:")
	if message == "" {
		message = fmt.Sprintf("transaction did not produce a verified result (process: %v; result: %v)", processErr, readErr)
	}
	if processErr != nil {
		message += fmt.Sprintf("; process: %v", processErr)
	}
	data, _ := json.Marshal(outcome{success, message})
	tmp := filepath.Join(stage, "worker-result.json.tmp")
	if err := os.WriteFile(tmp, data, 0600); err != nil {
		return err
	}
	if err := os.Rename(tmp, filepath.Join(stage, "worker-result.json")); err != nil {
		return err
	}
	if !success {
		return fmt.Errorf("%s", message)
	}
	return nil
}

func followWorker(cmd *exec.Cmd, stage string) error {
	finished := make(chan error, 1)
	go func() { finished <- cmd.Wait() }()
	ticker := time.NewTicker(time.Second)
	defer ticker.Stop()
	var offset int64
	flush := func() {
		f, err := os.Open(filepath.Join(stage, "worker.log"))
		if err != nil {
			return
		}
		defer f.Close()
		if _, err = f.Seek(offset, io.SeekStart); err == nil {
			n, _ := io.Copy(os.Stdout, f)
			offset += n
		}
	}
	started := time.Now()
	ticks := 0
	for {
		select {
		case err := <-finished:
			flush()
			data, readErr := os.ReadFile(filepath.Join(stage, "worker-result.json"))
			var result outcome
			if readErr != nil || json.Unmarshal(data, &result) != nil {
				return fmt.Errorf("worker exited without verified result (%v); inspect %s/worker.log", err, stage)
			}
			if err != nil || !result.Success {
				return fmt.Errorf("%s; logs/backups: %s", result.Message, stage)
			}
			fmt.Println("[5/5]", result.Message)
			if strings.HasPrefix(result.Message, "SUCCESS: session;") {
				fmt.Println("Session only: after reboot run sh /cache/zwrt-datad/start.sh")
			}
			fmt.Println("App: ZWRT mode, device LAN IP, port 9461, admin + original web password.")
			return nil
		case <-ticker.C:
			flush()
			ticks++
			if ticks%10 == 0 {
				fmt.Printf("Worker %d running (%ds); transaction output shown above.\n", cmd.Process.Pid, int(time.Since(started).Seconds()))
			}
		}
	}
}
