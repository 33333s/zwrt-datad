package main

import (
	"context"
	"crypto/ed25519"
	"crypto/sha256"
	"crypto/x509"
	_ "embed"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"time"
)

const (
	otaNetdisk = "https://pan.ericsfj.com/d/github%20releases/zwrt-datad"
	otaGitHub  = "https://github.com/33333s/zwrt-datad/releases/latest/download"
	otaIdleFor = 2 * time.Minute
)

var errWaitingIdle = errors.New("waiting for idle conditions")

//go:embed ota_public.pem
var otaPublicPEM []byte

type otaConfig struct {
	Enabled bool     `json:"enabled"`
	Servers []string `json:"servers"`
}

type otaArtifact struct {
	Name   string `json:"name"`
	Size   int64  `json:"size"`
	SHA256 string `json:"sha256"`
}

type otaManifest struct {
	Schema      int                    `json:"schema"`
	Version     string                 `json:"version"`
	Tag         string                 `json:"tag"`
	PublishedAt string                 `json:"published_at"`
	Artifacts   map[string]otaArtifact `json:"artifacts"`
}

type otaStatus struct {
	State          string   `json:"state"`
	CurrentVersion string   `json:"current_version"`
	LatestVersion  string   `json:"latest_version,omitempty"`
	Source         string   `json:"source,omitempty"`
	Progress       int      `json:"progress"`
	Error          string   `json:"error,omitempty"`
	LastCheckAt    int64    `json:"last_check_at,omitempty"`
	LastSuccessAt  int64    `json:"last_success_at,omitempty"`
	NextRetryAt    int64    `json:"next_retry_at,omitempty"`
	FailureCount   int      `json:"failure_count"`
	WaitReasons    []string `json:"wait_reasons,omitempty"`
	SignatureOK    bool     `json:"signature_verified"`
}

type otaCandidate struct {
	Manifest otaManifest
	BaseURL  string
}

type otaManager struct {
	mu            sync.Mutex
	dir           string
	stateURL      string
	config        otaConfig
	status        otaStatus
	candidate     *otaCandidate
	idleSince     time.Time
	busy          bool
	client        *http.Client
	pub           ed25519.PublicKey
	lastAutoCheck time.Time
}

func newOTAManager(dir, stateURL string) (*otaManager, error) {
	block, _ := pem.Decode(otaPublicPEM)
	if block == nil {
		return nil, errors.New("OTA 公钥无效")
	}
	parsed, err := x509.ParsePKIXPublicKey(block.Bytes)
	if err != nil {
		return nil, err
	}
	pub, ok := parsed.(ed25519.PublicKey)
	if !ok {
		return nil, errors.New("OTA 公钥类型无效")
	}
	o := &otaManager{dir: dir, stateURL: stateURL, config: otaConfig{Enabled: true},
		status: otaStatus{State: "idle", CurrentVersion: version},
		client: &http.Client{Timeout: 45 * time.Second}, pub: pub}
	if b, err := os.ReadFile(filepath.Join(dir, "ota.json")); err == nil {
		var cfg otaConfig
		if json.Unmarshal(b, &cfg) == nil && validateOTAConfig(cfg) == nil {
			o.config = cfg
		}
	}
	if b, err := os.ReadFile(filepath.Join(dir, "ota-state.json")); err == nil {
		var st otaStatus
		if json.Unmarshal(b, &st) == nil {
			st.CurrentVersion = version
			o.status = st
		}
	}
	return o, nil
}

func validateOTAConfig(c otaConfig) error {
	if len(c.Servers) > 8 {
		return errors.New("自定义更新服务器最多 8 个")
	}
	seen := map[string]bool{}
	for i, s := range c.Servers {
		s = strings.TrimRight(strings.TrimSpace(s), "/")
		u, err := url.Parse(s)
		if err != nil || u.Scheme != "https" || u.Host == "" || u.User != nil || u.RawQuery != "" || u.Fragment != "" {
			return fmt.Errorf("第 %d 个更新服务器必须是 HTTPS 地址", i+1)
		}
		if seen[s] {
			return errors.New("更新服务器地址重复")
		}
		seen[s] = true
	}
	return nil
}

func (o *otaManager) servers() []string {
	o.mu.Lock()
	custom := append([]string(nil), o.config.Servers...)
	o.mu.Unlock()
	out := make([]string, 0, len(custom)+2)
	seen := map[string]bool{}
	for _, s := range append(custom, otaNetdisk, otaGitHub) {
		s = strings.TrimRight(strings.TrimSpace(s), "/")
		if s != "" && !seen[s] {
			seen[s] = true
			out = append(out, s)
		}
	}
	return out
}

func (o *otaManager) saveConfig() error {
	b, _ := json.MarshalIndent(o.config, "", "  ")
	return otaAtomicWrite(filepath.Join(o.dir, "ota.json"), append(b, '\n'), 0600)
}
func (o *otaManager) saveStatusLocked() {
	b, _ := json.MarshalIndent(o.status, "", "  ")
	_ = otaAtomicWrite(filepath.Join(o.dir, "ota-state.json"), append(b, '\n'), 0600)
}
func otaAtomicWrite(path string, b []byte, mode os.FileMode) error {
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, b, mode); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

func (o *otaManager) configAPI(w http.ResponseWriter, r *http.Request) {
	if r.Method == http.MethodGet {
		o.mu.Lock()
		cfg := o.config
		o.mu.Unlock()
		writeOTAJSON(w, 200, map[string]any{"success": true, "config": cfg, "default_servers": []string{otaNetdisk, otaGitHub}})
		return
	}
	if r.Method != http.MethodPost {
		writeOTAJSON(w, 405, map[string]any{"success": false, "error": "method not allowed"})
		return
	}
	var cfg otaConfig
	if err := json.NewDecoder(io.LimitReader(r.Body, 16<<10)).Decode(&cfg); err != nil || validateOTAConfig(cfg) != nil {
		if err == nil {
			err = validateOTAConfig(cfg)
		}
		writeOTAJSON(w, 400, map[string]any{"success": false, "error": err.Error()})
		return
	}
	for i := range cfg.Servers {
		cfg.Servers[i] = strings.TrimRight(strings.TrimSpace(cfg.Servers[i]), "/")
	}
	o.mu.Lock()
	o.config = cfg
	err := o.saveConfig()
	o.mu.Unlock()
	if err != nil {
		writeOTAJSON(w, 500, map[string]any{"success": false, "error": err.Error()})
		return
	}
	writeOTAJSON(w, 200, map[string]any{"success": true, "config": cfg})
}

func (o *otaManager) statusAPI(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet {
		writeOTAJSON(w, 405, map[string]any{"success": false, "error": "method not allowed"})
		return
	}
	o.mu.Lock()
	st := o.status
	cfg := o.config
	o.mu.Unlock()
	writeOTAJSON(w, 200, map[string]any{"success": true, "status": st, "enabled": cfg.Enabled})
}

func (o *otaManager) checkAPI(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		writeOTAJSON(w, 405, map[string]any{"success": false, "error": "method not allowed"})
		return
	}
	c, err := o.check(r.Context())
	if err != nil {
		writeOTAJSON(w, 502, map[string]any{"success": false, "error": err.Error()})
		return
	}
	writeOTAJSON(w, 200, map[string]any{"success": true, "has_update": newer(c.Manifest.Version, version), "manifest": c.Manifest, "source": c.BaseURL})
}
func (o *otaManager) updateAPI(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		writeOTAJSON(w, 405, map[string]any{"success": false, "error": "method not allowed"})
		return
	}
	o.mu.Lock()
	if o.busy {
		o.mu.Unlock()
		writeOTAJSON(w, 409, map[string]any{"success": false, "error": "更新任务正在运行"})
		return
	}
	o.busy = true
	o.status.State = "checking"
	o.status.Error = ""
	o.saveStatusLocked()
	o.mu.Unlock()
	go func() {
		defer func() { o.mu.Lock(); o.busy = false; o.mu.Unlock() }()
		c, err := o.check(context.Background())
		if err == nil && !newer(c.Manifest.Version, version) {
			err = errors.New("当前已是最新版本")
		}
		if err == nil {
			err = o.install(c, true)
		}
		if err != nil {
			o.fail(c, err)
		}
	}()
	writeOTAJSON(w, 202, map[string]any{"success": true, "status": "started"})
}

func (o *otaManager) check(ctx context.Context) (*otaCandidate, error) {
	o.mu.Lock()
	o.status.State = "checking"
	o.status.Error = ""
	o.status.LastCheckAt = time.Now().Unix()
	o.status.Progress = 0
	o.saveStatusLocked()
	o.mu.Unlock()
	var errs []string
	for _, base := range o.servers() {
		murl := base + "/update.json"
		raw, err := o.fetch(ctx, murl, 1<<20)
		if err != nil {
			errs = append(errs, base+": "+err.Error())
			continue
		}
		sigb, err := o.fetch(ctx, base+"/update.json.sig", 4096)
		if err != nil {
			errs = append(errs, base+": "+err.Error())
			continue
		}
		sig, err := base64.StdEncoding.DecodeString(strings.TrimSpace(string(sigb)))
		if err != nil || !ed25519.Verify(o.pub, raw, sig) {
			errs = append(errs, base+": 签名校验失败")
			continue
		}
		var m otaManifest
		if json.Unmarshal(raw, &m) != nil || validateManifest(m) != nil {
			errs = append(errs, base+": 清单无效")
			continue
		}
		c := &otaCandidate{Manifest: m, BaseURL: base}
		o.mu.Lock()
		o.candidate = c
		o.status.LatestVersion = m.Version
		o.status.Source = base
		o.status.SignatureOK = true
		o.status.State = "idle"
		if newer(m.Version, version) {
			o.status.State = "available"
		}
		o.saveStatusLocked()
		o.mu.Unlock()
		return c, nil
	}
	err := errors.New("所有更新服务器均不可用或签名无效: " + strings.Join(errs, "; "))
	o.mu.Lock()
	o.status.State = "error"
	o.status.Error = err.Error()
	o.saveStatusLocked()
	o.mu.Unlock()
	return nil, err
}

func validateManifest(m otaManifest) error {
	if m.Schema != 1 || !regexp.MustCompile(`^\d+\.\d+\.\d+$`).MatchString(m.Version) || m.Tag != "v"+m.Version {
		return errors.New("版本清单无效")
	}
	for _, k := range []string{"installer", "binary"} {
		a, ok := m.Artifacts[k]
		if !ok || a.Size <= 0 || !regexp.MustCompile(`^[a-f0-9]{64}$`).MatchString(a.SHA256) || strings.ContainsAny(a.Name, "/\\") {
			return errors.New("产物清单无效")
		}
	}
	return nil
}
func (o *otaManager) fetch(ctx context.Context, u string, max int64) ([]byte, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, u, nil)
	if err != nil {
		return nil, err
	}
	resp, err := o.client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("HTTP %d", resp.StatusCode)
	}
	b, err := io.ReadAll(io.LimitReader(resp.Body, max+1))
	if int64(len(b)) > max {
		return nil, errors.New("响应过大")
	}
	return b, err
}

func (o *otaManager) install(c *otaCandidate, manual bool) error {
	reasons, err := o.safety(manual)
	if err != nil {
		return err
	}
	if len(reasons) > 0 {
		o.mu.Lock()
		o.status.State = "waiting_idle"
		o.status.WaitReasons = reasons
		o.saveStatusLocked()
		o.mu.Unlock()
		if !manual {
			return errWaitingIdle
		}
		return errors.New("等待安装条件: " + strings.Join(reasons, "、"))
	}
	a := c.Manifest.Artifacts["installer"]
	o.mu.Lock()
	o.status.State = "downloading"
	o.status.Progress = 5
	o.status.WaitReasons = nil
	o.saveStatusLocked()
	o.mu.Unlock()
	raw, err := o.fetch(context.Background(), c.BaseURL+"/"+url.PathEscape(a.Name), 16<<20)
	if err != nil {
		return err
	}
	sum := sha256.Sum256(raw)
	if hex.EncodeToString(sum[:]) != a.SHA256 {
		return errors.New("安装器 SHA-256 校验失败")
	}
	path := filepath.Join(o.dir, "ota-installer.sh")
	if err = otaAtomicWrite(path, raw, 0700); err != nil {
		return err
	}
	b := c.Manifest.Artifacts["binary"]
	wrapper := filepath.Join(o.dir, "ota-run.sh")
	result := filepath.Join(o.dir, "ota-result.log")
	marker := filepath.Join(o.dir, "ota-install-result")
	script := "#!/bin/sh\nsleep 2\nrm -f " + shellQuote(marker) + "\nif DATAD_DOWNLOAD_URL=" + shellQuote(c.BaseURL+"/"+url.PathEscape(b.Name)) + " sh " + shellQuote(path) + " >" + shellQuote(result) + " 2>&1; then value=success; else value=failed; fi\nprintf '%s\\n' \"$value\" >" + shellQuote(marker+".tmp") + "\nmv -f " + shellQuote(marker+".tmp") + " " + shellQuote(marker) + "\n"
	if err = otaAtomicWrite(wrapper, []byte(script), 0700); err != nil {
		return err
	}
	o.mu.Lock()
	o.status.State = "installing"
	o.status.Progress = 100
	o.saveStatusLocked()
	o.mu.Unlock()
	cmd := exec.Command("/bin/sh", wrapper)
	cmd.Stdin = nil
	cmd.Stdout = nil
	cmd.Stderr = nil
	if err = cmd.Start(); err != nil {
		return err
	}
	return cmd.Process.Release()
}
func shellQuote(s string) string { return "'" + strings.ReplaceAll(s, "'", "'\\''") + "'" }

func (o *otaManager) safety(manual bool) ([]string, error) {
	resp, err := o.client.Get(o.stateURL)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	var s map[string]any
	if json.NewDecoder(io.LimitReader(resp.Body, 4<<20)).Decode(&s) != nil {
		return nil, errors.New("无法读取设备状态")
	}
	var reasons []string
	battery := numberAt(s, "battery", "percent")
	if battery < 0 {
		battery = numberAt(s, "system", "battery_percent")
	}
	if battery >= 0 && battery <= 10 {
		reasons = append(reasons, "电量必须高于 10%")
	}
	storage := numberAt(s, "runtime", "storage", "available")
	if storage >= 0 && storage < 64*1024*1024 {
		reasons = append(reasons, "可用存储不足 64 MiB")
	}
	if manual {
		return reasons, nil
	}
	if boolAt(s, "neighbor", "enabled") || boolAt(s, "neighbor", "collector_running") {
		reasons = append(reasons, "请先关闭邻区采集")
	}
	cpu := numberAt(s, "runtime", "cpu_usage_tenths")
	if cpu >= 0 {
		cpu /= 10
	} else {
		cpu = numberAt(s, "system", "cpu_usage")
	}
	if cpu >= 60 {
		reasons = append(reasons, "CPU 占用需低于 60%")
	}
	rate := numberAt(s, "runtime", "throughput", "rx_bps") + numberAt(s, "runtime", "throughput", "tx_bps")
	if rate >= 1024*1024 {
		reasons = append(reasons, "上下行需低于 1 MiB/s")
	}
	o.mu.Lock()
	defer o.mu.Unlock()
	if len(reasons) > 0 {
		o.idleSince = time.Time{}
		return reasons, nil
	}
	if o.idleSince.IsZero() {
		o.idleSince = time.Now()
	}
	if time.Since(o.idleSince) < otaIdleFor {
		reasons = append(reasons, "设备需连续空闲 2 分钟")
	}
	return reasons, nil
}

func numberAt(m map[string]any, path ...string) float64 {
	var v any = m
	for _, p := range path {
		x, ok := v.(map[string]any)
		if !ok {
			return -1
		}
		v = x[p]
	}
	switch n := v.(type) {
	case float64:
		return n
	case json.Number:
		f, _ := n.Float64()
		return f
	}
	return -1
}
func boolAt(m map[string]any, path ...string) bool {
	var v any = m
	for _, p := range path {
		x, ok := v.(map[string]any)
		if !ok {
			return false
		}
		v = x[p]
	}
	b, _ := v.(bool)
	return b
}
func newer(a, b string) bool {
	pa := strings.Split(a, ".")
	pb := strings.Split(b, ".")
	for i := 0; i < 3; i++ {
		var x, y int
		fmt.Sscanf(pa[i], "%d", &x)
		fmt.Sscanf(pb[i], "%d", &y)
		if x != y {
			return x > y
		}
	}
	return false
}
func (o *otaManager) fail(c *otaCandidate, err error) {
	o.mu.Lock()
	defer o.mu.Unlock()
	o.status.State = "error"
	o.status.Error = err.Error()
	o.status.FailureCount++
	delays := []time.Duration{time.Hour, 6 * time.Hour, 24 * time.Hour}
	if o.status.FailureCount <= len(delays) {
		o.status.NextRetryAt = time.Now().Add(delays[o.status.FailureCount-1]).Unix()
	} else {
		o.status.State = "blocked"
		o.status.NextRetryAt = 0
	}
	if c != nil {
		o.status.LatestVersion = c.Manifest.Version
		o.status.Source = c.BaseURL
	}
	o.saveStatusLocked()
}

func (o *otaManager) run(ctx context.Context) {
	timer := time.NewTimer(90 * time.Second)
	defer timer.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-timer.C:
			o.reconcileInstallResult()
			o.auto()
			timer.Reset(time.Minute)
		}
	}
}

func (o *otaManager) reconcileInstallResult() {
	b, err := os.ReadFile(filepath.Join(o.dir, "ota-install-result"))
	if err != nil {
		return
	}
	_ = os.Remove(filepath.Join(o.dir, "ota-install-result"))
	o.mu.Lock()
	defer o.mu.Unlock()
	if strings.TrimSpace(string(b)) == "success" {
		o.status.State = "succeeded"
		o.status.CurrentVersion = version
		o.status.LastSuccessAt = time.Now().Unix()
		o.status.Error = ""
		o.status.FailureCount = 0
		o.status.NextRetryAt = 0
	} else {
		o.status.State = "error"
		o.status.Error = "安装器执行失败，已由安装器回滚"
		o.status.FailureCount++
	}
	o.saveStatusLocked()
}
func (o *otaManager) auto() {
	o.mu.Lock()
	enabled := o.config.Enabled
	busy := o.busy
	blocked := o.status.State == "blocked"
	candidate := o.candidate
	nextRetry := o.status.NextRetryAt
	lastCheck := o.lastAutoCheck
	o.mu.Unlock()
	if !enabled || busy {
		return
	}
	if nextRetry > time.Now().Unix() {
		return
	}
	c := candidate
	if c == nil || time.Since(lastCheck) >= 6*time.Hour {
		var err error
		c, err = o.check(context.Background())
		o.mu.Lock()
		o.lastAutoCheck = time.Now()
		o.mu.Unlock()
		if err != nil {
			return
		}
	}
	if blocked {
		o.mu.Lock()
		blocked = o.status.LatestVersion == c.Manifest.Version
		o.mu.Unlock()
		if blocked {
			return
		}
	}
	if !newer(c.Manifest.Version, version) {
		return
	}
	o.mu.Lock()
	o.busy = true
	o.mu.Unlock()
	defer func() { o.mu.Lock(); o.busy = false; o.mu.Unlock() }()
	if err := o.install(c, false); err != nil && !errors.Is(err, errWaitingIdle) {
		o.fail(c, err)
	}
}
func writeOTAJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}
