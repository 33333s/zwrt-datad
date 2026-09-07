package main

import (
	"encoding/json"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func validConfig() Config {
	c := defaults()
	c.Enabled = true
	c.Broker = "ssl://nms.example.com:4003"
	c.PlatformURL = "https://nms.example.com:4002"
	c.Username = "device"
	c.Password = "secret"
	c.Model = "MU5252"
	c.Identity = "test-device"
	return c
}
func TestValidation(t *testing.T) {
	c := validConfig()
	if e := validate(c); e != nil {
		t.Fatal(e)
	}
	for _, mutate := range []func(*Config){func(c *Config) { c.Broker = "tcp://host:1883" }, func(c *Config) { c.Broker = "ssl://user:secret@host:8883" }, func(c *Config) { c.Model = "+/x" }, func(c *Config) { c.Services[0].Port = 9460 }, func(c *Config) { c.CAPEM = "invalid" }, func(c *Config) { c.Interval = 0 }} {
		c = validConfig()
		mutate(&c)
		if validate(c) == nil {
			t.Fatal("accepted invalid config")
		}
	}
}
func TestConfigRoundtrip(t *testing.T) {
	a := &agent{config: validConfig(), file: filepath.Join(t.TempDir(), "cloud.json"), change: make(chan struct{}, 1)}
	c := a.config
	c.Password = ""
	c.Enabled = false
	b, _ := json.Marshal(c)
	w := httptest.NewRecorder()
	a.configAPI(w, httptest.NewRequest("POST", "/cloud/config", strings.NewReader(string(b))))
	if w.Code != 200 || strings.Contains(w.Body.String(), "secret") {
		t.Fatal(w.Code, w.Body.String())
	}
	if a.config.Password != "secret" || a.config.Enabled {
		t.Fatal("save semantics")
	}
	info, _ := os.Stat(a.file)
	if info.Mode().Perm() != 0600 {
		t.Fatal(info.Mode())
	}
	w = httptest.NewRecorder()
	a.configAPI(w, httptest.NewRequest("POST", "/cloud/config", strings.NewReader(`{"unknown":true}`)))
	if w.Code != 400 {
		t.Fatal(w.Code)
	}
	w = httptest.NewRecorder()
	a.configAPI(w, httptest.NewRequest("GET", "/cloud/config", nil))
	if strings.Contains(w.Body.String(), "secret") {
		t.Fatal("secret leaked")
	}
}
func TestRemoteBoundary(t *testing.T) {
	c := validConfig()
	c.RemoteEnabled = true
	cmd := remoteCommand{Version: 1, ID: "session-123", Action: "remote.open", URL: "wss://nms.example.com:4002/api/remote/device/session-123", Token: strings.Repeat("a", 64), Service: "router_web", Port: 2333, TTL: 900}
	if e := validateRemote(c, cmd); e != nil {
		t.Fatal(e)
	}
	for _, mutate := range []func(*remoteCommand){func(x *remoteCommand) { x.URL = "wss://evil.example/api/remote/device/session-123" }, func(x *remoteCommand) { x.Port = 9460 }, func(x *remoteCommand) { x.Ports = []int{2333, 81} }, func(x *remoteCommand) { x.TTL = 0 }, func(x *remoteCommand) { x.Service = "terminal" }} {
		bad := cmd
		mutate(&bad)
		if validateRemote(c, bad) == nil {
			t.Fatal("unsafe remote accepted")
		}
	}
	c.RemoteEnabled = false
	if validateRemote(c, cmd) == nil {
		t.Fatal("disabled remote accepted")
	}
}
