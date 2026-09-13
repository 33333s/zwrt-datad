package main

import (
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func signedOTAServer(t *testing.T, key ed25519.PrivateKey, version string) *httptest.Server {
	t.Helper()
	installer := []byte("#!/bin/sh\nexit 0\n")
	binary := []byte("binary")
	ih, bh := sha256.Sum256(installer), sha256.Sum256(binary)
	m := otaManifest{Schema: 1, Version: version, Tag: "v" + version, PublishedAt: "2026-09-14T00:00:00Z", Artifacts: map[string]otaArtifact{
		"installer": {Name: "install-datad.sh", Size: int64(len(installer)), SHA256: hex.EncodeToString(ih[:])},
		"binary":    {Name: "zwrt-datad-aarch64", Size: int64(len(binary)), SHA256: hex.EncodeToString(bh[:])},
	}}
	raw, _ := json.Marshal(m)
	raw = append(raw, '\n')
	sig := base64.StdEncoding.EncodeToString(ed25519.Sign(key, raw))
	return httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/update.json":
			_, _ = w.Write(raw)
		case "/update.json.sig":
			_, _ = w.Write([]byte(sig))
		case "/install-datad.sh":
			_, _ = w.Write(installer)
		default:
			http.NotFound(w, r)
		}
	}))
}

func TestNetdiskUsesPerFileSignedURLs(t *testing.T) {
	for _, name := range []string{"update.json", "update.json.sig", "install-datad.sh", "zwrt-datad-aarch64"} {
		u := sourceURL(otaNetdisk, name)
		if !strings.HasPrefix(u, "https://") || !strings.Contains(u, "sign=") {
			t.Fatalf("%s does not use a signed URL", name)
		}
	}
	if got := sourceURL("https://custom.example/base", "update.json"); got != "https://custom.example/base/update.json" {
		t.Fatal(got)
	}
}

func TestOTASignedManifestAndCustomPriority(t *testing.T) {
	pub, priv, _ := ed25519.GenerateKey(rand.Reader)
	good := signedOTAServer(t, priv, "99.0.0")
	defer good.Close()
	bad := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { _, _ = w.Write([]byte("tampered")) }))
	defer bad.Close()
	o := &otaManager{dir: t.TempDir(), config: otaConfig{Enabled: true, Servers: []string{bad.URL, good.URL}}, status: otaStatus{State: "waiting_idle", WaitReasons: []string{"设备需连续空闲 2 分钟"}}, client: good.Client(), pub: pub}
	o.client.Timeout = 2 * time.Second
	c, err := o.check(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if c.BaseURL != good.URL || c.Manifest.Version != "99.0.0" {
		t.Fatalf("unexpected candidate: %#v", c)
	}
	o.mu.Lock()
	verified := o.status.SignatureOK
	waitReasons := append([]string(nil), o.status.WaitReasons...)
	o.mu.Unlock()
	if !verified {
		t.Fatal("signature was not recorded")
	}
	if len(waitReasons) != 0 {
		t.Fatalf("successful check retained stale wait reasons: %v", waitReasons)
	}
}

func TestOTARejectsChangedSignedManifest(t *testing.T) {
	pub, priv, _ := ed25519.GenerateKey(rand.Reader)
	server := signedOTAServer(t, priv, "1.2.3")
	defer server.Close()
	o := &otaManager{dir: t.TempDir(), config: otaConfig{Enabled: true, Servers: []string{server.URL}}, status: otaStatus{State: "idle"}, client: server.Client(), pub: pub}
	o.pub[0] ^= 1
	_, err := o.check(context.Background())
	if err == nil || !strings.Contains(err.Error(), "签名") {
		t.Fatalf("expected signature error, got %v", err)
	}
}

func TestOTAConfigDefaultsEnabledAndPersists(t *testing.T) {
	pub, _, _ := ed25519.GenerateKey(rand.Reader)
	dir := t.TempDir()
	o := &otaManager{dir: dir, config: otaConfig{Enabled: true}, status: otaStatus{State: "idle"}, pub: pub}
	req := httptest.NewRequest(http.MethodPost, "/ota/config", strings.NewReader(`{"enabled":false,"servers":["https://updates.example/a"]}`))
	w := httptest.NewRecorder()
	o.configAPI(w, req)
	if w.Code != 200 {
		t.Fatal(w.Body.String())
	}
	b, err := osRead(filepath.Join(dir, "ota.json"))
	if err != nil || !strings.Contains(string(b), "updates.example") {
		t.Fatalf("config not persisted: %v %s", err, b)
	}
}

func TestAutomaticIdleWaitDoesNotCountAsFailure(t *testing.T) {
	state := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(`{"battery":{"percent":80},"neighbor":{"enabled":false},"runtime":{"cpu_usage_tenths":300,"storage":{"available":268435456},"throughput":{"rx_bps":100,"tx_bps":100}}}`))
	}))
	defer state.Close()
	o := &otaManager{dir: t.TempDir(), stateURL: state.URL, config: otaConfig{Enabled: true}, status: otaStatus{State: "available"}, client: state.Client()}
	err := o.install(&otaCandidate{}, false)
	if !errors.Is(err, errWaitingIdle) {
		t.Fatalf("expected idle wait, got %v", err)
	}
	if o.status.FailureCount != 0 || o.status.State != "waiting_idle" {
		t.Fatalf("idle wait counted as failure: %#v", o.status)
	}
}

func TestOTAReconcileSuccessClearsWaitReasons(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "ota-install-result"), []byte("success\n"), 0600); err != nil {
		t.Fatal(err)
	}
	o := &otaManager{dir: dir, status: otaStatus{State: "waiting_idle", Error: "old error", WaitReasons: []string{"设备需连续空闲 2 分钟"}, FailureCount: 2, NextRetryAt: 123}}
	o.reconcileInstallResult()
	if o.status.State != "succeeded" || o.status.Error != "" || len(o.status.WaitReasons) != 0 || o.status.FailureCount != 0 || o.status.NextRetryAt != 0 {
		t.Fatalf("unexpected reconciled status: %#v", o.status)
	}
}

func osRead(path string) ([]byte, error) { return os.ReadFile(path) }
