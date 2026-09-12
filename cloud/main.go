// The cloud runtime and C sampler are linked into one zwrt-datad executable.
package main

import (
	"context"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"time"

	mqtt "github.com/eclipse/paho.mqtt.golang"
)

var version = "dev"
var topicPart = regexp.MustCompile(`^[A-Za-z0-9_. -]{1,128}$`)

type Service struct {
	Name string `json:"name"`
	Port int    `json:"port"`
	Kind string `json:"kind"`
}
type Config struct {
	Enabled       bool      `json:"enabled"`
	Broker        string    `json:"broker"`
	PlatformURL   string    `json:"platform_url"`
	Username      string    `json:"username"`
	Password      string    `json:"password,omitempty"`
	CAPEM         string    `json:"ca_pem,omitempty"`
	Vendor        string    `json:"vendor"`
	Model         string    `json:"model"`
	IdentityType  string    `json:"identity_type"`
	Identity      string    `json:"identity"`
	Platform      string    `json:"platform"`
	Interval      int       `json:"report_interval_seconds"`
	RemoteEnabled bool      `json:"remote_enabled"`
	Services      []Service `json:"services"`
}
type status struct {
	State      string `json:"state"`
	Error      string `json:"error,omitempty"`
	LastReport int64  `json:"last_report_at,omitempty"`
}
type agent struct {
	mu        sync.Mutex
	config    Config
	status    status
	loadError bool
	file      string
	stateURL  string
	change    chan struct{}
}

func defaults() Config {
	return Config{Vendor: "ZTE", IdentityType: "uuid", Platform: "qualcomm", Interval: 30,
		Services: []Service{{"设备后台", 80, "web"}, {"UFI", 2333, "web"}, {"WebSSH", 8899, "terminal"}}}
}
func validate(c Config) error {
	if c.Interval < 10 || c.Interval > 3600 {
		return errors.New("上报间隔必须为 10–3600 秒")
	}
	if len(c.Services) > 8 {
		return errors.New("最多配置 8 个后台")
	}
	ports := map[int]bool{}
	for _, s := range c.Services {
		if s.Port < 1 || s.Port > 65535 || s.Port == 9460 || s.Port == 9461 || ports[s.Port] || strings.TrimSpace(s.Name) == "" || len(s.Name) > 80 || (s.Kind != "web" && s.Kind != "terminal") {
			return errors.New("后台名称、类型或端口无效，不能使用 datad 管理端口")
		}
		ports[s.Port] = true
	}
	if c.Broker != "" {
		u, e := url.Parse(c.Broker)
		if e != nil || u.Scheme != "ssl" || u.Hostname() == "" || u.Port() == "" || u.User != nil || u.Path != "" || u.RawQuery != "" || u.Fragment != "" {
			return errors.New("MQTT 地址格式为 ssl://主机:端口")
		}
		if _, e := net.LookupPort("tcp", u.Port()); e != nil {
			return errors.New("MQTT 端口无效")
		}
	}
	if c.PlatformURL != "" {
		u, e := url.Parse(c.PlatformURL)
		if e != nil || u.Scheme != "https" || u.Hostname() == "" || u.User != nil || (u.Path != "" && u.Path != "/") || u.RawQuery != "" || u.Fragment != "" {
			return errors.New("NMS 地址必须是 HTTPS 站点地址")
		}
	}
	if _, e := tlsConfig(c); e != nil {
		return e
	}
	if c.Enabled {
		if c.Broker == "" || c.Username == "" || c.Password == "" {
			return errors.New("请填写 MQTT 地址和设备凭据")
		}
		if !topicPart.MatchString(c.Vendor) || !topicPart.MatchString(c.Model) || !topicPart.MatchString(c.Identity) {
			return errors.New("厂商、型号和设备标识不能为空或包含主题特殊字符")
		}
		if c.IdentityType != "uuid" && c.IdentityType != "sn" {
			return errors.New("设备标识类型无效")
		}
		if c.Platform != "generic" && c.Platform != "qualcomm" && c.Platform != "mediatek" && c.Platform != "quecopen" {
			return errors.New("固件平台无效")
		}
		if c.RemoteEnabled && (c.PlatformURL == "" || len(c.Services) == 0) {
			return errors.New("远程访问需要 NMS 地址和至少一个后台")
		}
	}
	return nil
}
func tlsConfig(c Config) (*tls.Config, error) {
	t := &tls.Config{MinVersion: tls.VersionTLS12}
	if c.CAPEM != "" {
		roots, e := x509.SystemCertPool()
		if e != nil {
			roots = x509.NewCertPool()
		}
		if !roots.AppendCertsFromPEM([]byte(c.CAPEM)) {
			return nil, errors.New("CA 证书无效")
		}
		t.RootCAs = roots
	}
	return t, nil
}
func (a *agent) setStatus(state, err string) {
	a.mu.Lock()
	a.status.State = state
	a.status.Error = err
	a.mu.Unlock()
}
func (a *agent) configAPI(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Cache-Control", "no-store")
	a.mu.Lock()
	defer a.mu.Unlock()
	if r.Method == "POST" {
		var incoming struct {
			Config
			ClearPassword bool `json:"clear_password"`
		}
		dec := json.NewDecoder(http.MaxBytesReader(w, r.Body, 32768))
		dec.DisallowUnknownFields()
		if dec.Decode(&incoming) != nil {
			http.Error(w, `{"error":"配置格式无效"}`, 400)
			return
		}
		var extra any
		if dec.Decode(&extra) != io.EOF {
			http.Error(w, `{"error":"配置格式无效"}`, 400)
			return
		}
		c := incoming.Config
		if c.Password == "" && !incoming.ClearPassword {
			c.Password = a.config.Password
		}
		if e := validate(c); e != nil {
			w.WriteHeader(400)
			json.NewEncoder(w).Encode(map[string]string{"error": e.Error()})
			return
		}
		b, e := json.MarshalIndent(c, "", "  ")
		if e == nil {
			e = atomicWrite(a.file, b)
		}
		if e != nil {
			http.Error(w, `{"error":"保存配置失败"}`, 500)
			return
		}
		a.config = c
		a.loadError = false
		a.status = status{State: "reconfiguring"}
		select {
		case a.change <- struct{}{}:
		default:
		}
	} else if r.Method != "GET" {
		w.WriteHeader(405)
		return
	}
	c := a.config
	hasPassword := c.Password != ""
	c.Password = ""
	json.NewEncoder(w).Encode(map[string]any{"config": c, "password_configured": hasPassword})
}
func atomicWrite(path string, b []byte) error {
	f, e := os.CreateTemp(filepath.Dir(path), ".cloud-*")
	if e != nil {
		return e
	}
	defer os.Remove(f.Name())
	if e = f.Chmod(0600); e == nil {
		_, e = f.Write(b)
	}
	if e == nil {
		e = f.Sync()
	}
	ce := f.Close()
	if e != nil {
		return e
	}
	if ce != nil {
		return ce
	}
	return os.Rename(f.Name(), path)
}
func (a *agent) statusAPI(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Cache-Control", "no-store")
	if r.Method != "GET" {
		w.WriteHeader(405)
		return
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	json.NewEncoder(w).Encode(a.status)
}
func root(c Config) string { return "devices/" + c.Vendor + "/" + c.Model + "/" + c.Identity }
func envelope(p map[string]any) map[string]any {
	p["protocol_version"] = 1
	p["timestamp"] = time.Now().Unix()
	return p
}
func publish(c mqtt.Client, topic string, p map[string]any) error {
	b, e := json.Marshal(envelope(p))
	if e != nil {
		return e
	}
	t := c.Publish(topic, 1, false, b)
	if !t.WaitTimeout(5 * time.Second) {
		return errors.New("publish timeout")
	}
	return t.Error()
}
func (a *agent) run(ctx context.Context) {
	for {
		a.mu.Lock()
		c := a.config
		loadError := a.loadError
		a.mu.Unlock()
		if loadError {
			select {
			case <-ctx.Done():
				return
			case <-a.change:
				continue
			}
		}
		runCtx, cancel := context.WithCancel(ctx)
		done := make(chan struct{})
		go func() { defer close(done); a.connect(runCtx, c) }()
		select {
		case <-ctx.Done():
			cancel()
			<-done
			return
		case <-a.change:
			cancel()
			<-done
		}
	}
}
func (a *agent) connect(ctx context.Context, c Config) {
	if !c.Enabled {
		a.setStatus("disabled", "")
		<-ctx.Done()
		return
	}
	if e := validate(c); e != nil {
		a.setStatus("error", e.Error())
		<-ctx.Done()
		return
	}
	delay := time.Second
	for ctx.Err() == nil {
		a.setStatus("connecting", "")
		if a.session(ctx, c) == nil {
			return
		}
		if ctx.Err() != nil {
			return
		}
		a.setStatus("retrying", "云端连接中断或认证失败，请检查地址、证书与设备凭据")
		timer := time.NewTimer(delay)
		select {
		case <-ctx.Done():
			timer.Stop()
			return
		case <-timer.C:
		}
		if delay < 30*time.Second {
			delay *= 2
		}
		if delay > 30*time.Second {
			delay = 30 * time.Second
		}
	}
}
func (a *agent) session(ctx context.Context, c Config) error {
	tlsCfg, _ := tlsConfig(c)
	sum := sha256.Sum256([]byte(root(c)))
	opts := mqtt.NewClientOptions().AddBroker(c.Broker).SetClientID("datad-" + hex.EncodeToString(sum[:12])).SetUsername(c.Username).SetPassword(c.Password).SetTLSConfig(tlsCfg).SetConnectTimeout(5 * time.Second).SetKeepAlive(30 * time.Second).SetPingTimeout(5 * time.Second).SetAutoReconnect(false).SetCleanSession(true).SetWriteTimeout(5 * time.Second)
	will, _ := json.Marshal(envelope(map[string]any{"online": false}))
	opts.SetWill(root(c)+"/status", string(will), 1, false)
	lost := make(chan struct{}, 1)
	opts.SetConnectionLostHandler(func(mqtt.Client, error) {
		select {
		case lost <- struct{}{}:
		default:
		}
	})
	client := mqtt.NewClient(opts)
	token := client.Connect()
	token.Wait()
	if token.Error() != nil {
		return token.Error()
	}
	defer client.Disconnect(100)
	bridgeCtx, closeBridges := context.WithCancel(ctx)
	defer closeBridges()
	manager := newBridgeManager(bridgeCtx, c)
	sub := client.Subscribe(root(c)+"/command/request", 1, func(_ mqtt.Client, m mqtt.Message) { manager.receive(m, client) })
	if !sub.WaitTimeout(5*time.Second) || sub.Error() != nil {
		return errors.New("subscribe failed")
	}
	a.setStatus("connected", "")
	ticker := time.NewTicker(time.Duration(c.Interval) * time.Second)
	defer ticker.Stop()
	for {
		if e := a.report(ctx, client, c); e != nil {
			return e
		}
		select {
		case <-ctx.Done():
			publish(client, root(c)+"/status", map[string]any{"online": false})
			return nil
		case <-lost:
			return errors.New("connection lost")
		case <-ticker.C:
		}
	}
}
func (a *agent) report(ctx context.Context, client mqtt.Client, c Config) error {
	req, _ := http.NewRequestWithContext(ctx, "GET", a.stateURL, nil)
	res, e := (&http.Client{Timeout: 3 * time.Second}).Do(req)
	var state map[string]any
	if e == nil {
		defer res.Body.Close()
		if res.StatusCode == 200 {
			e = json.NewDecoder(io.LimitReader(res.Body, 2<<20)).Decode(&state)
		} else {
			e = errors.New("sampler unavailable")
		}
	}
	if e != nil {
		a.setStatus("connected", "本地状态暂不可用")
	} else {
		a.setStatus("connected", "")
	}
	rawSystem, _ := state["system"].(map[string]any)
	fw, _ := rawSystem["sw_version"].(string)
	if fw == "" {
		fw, _ = rawSystem["fw"].(string)
	}
	if e = publish(client, root(c)+"/telemetry/device", map[string]any{"vendor": c.Vendor, "model": c.Model, "device_id": c.Identity, "id_type": c.IdentityType, "platform": c.Platform, "agent_version": version, "firmware_version": fw, "capabilities": []string{}, "remote_services": c.Services, "remote_enabled": c.RemoteEnabled}); e != nil {
		return e
	}
	if e = publish(client, root(c)+"/status", map[string]any{"online": true}); e != nil {
		return e
	}
	// Upload only selected non-secret resource metrics, never raw /state or modem credentials.
	system := systemTelemetry(state)
	boot, _ := os.ReadFile("/proc/sys/kernel/random/boot_id")
	system["boot_id"] = strings.TrimSpace(string(boot))
	up, _ := os.ReadFile("/proc/uptime")
	var uptime float64
	fmt.Sscanf(string(up), "%f", &uptime)
	system["uptime"] = uptime
	if e = publish(client, root(c)+"/telemetry/system", system); e != nil {
		return e
	}
	a.mu.Lock()
	a.status.LastReport = time.Now().Unix()
	a.mu.Unlock()
	return nil
}
