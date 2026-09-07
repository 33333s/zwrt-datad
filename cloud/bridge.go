package main

import (
	"context"
	"encoding/json"
	"errors"
	mqtt "github.com/eclipse/paho.mqtt.golang"
	"github.com/gorilla/websocket"
	"io"
	"net"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"time"
)

type remoteCommand struct {
	Version int    `json:"protocol_version"`
	ID      string `json:"request_id"`
	Action  string `json:"action"`
	URL     string `json:"remote_url"`
	Token   string `json:"token"`
	Service string `json:"target_service"`
	Port    int    `json:"target_port"`
	Ports   []int  `json:"target_ports"`
	TTL     int    `json:"ttl_seconds"`
}
type bridgeManager struct {
	ctx    context.Context
	config Config
	mu     sync.Mutex
	active map[string]context.CancelFunc
	seen   map[string]time.Time
}

func newBridgeManager(ctx context.Context, c Config) *bridgeManager {
	return &bridgeManager{ctx: ctx, config: c, active: map[string]context.CancelFunc{}, seen: map[string]time.Time{}}
}
func validateRemote(c Config, cmd remoteCommand) error {
	if !c.Enabled || !c.RemoteEnabled {
		return errors.New("remote_disabled")
	}
	if cmd.Version != 1 || cmd.Action != "remote.open" || !topicPart.MatchString(cmd.ID) || cmd.TTL < 1 || cmd.TTL > 43200 || len(cmd.Token) != 64 {
		return errors.New("invalid_request")
	}
	u, e := url.Parse(cmd.URL)
	base, _ := url.Parse(c.PlatformURL)
	if e != nil || base == nil || u.Scheme != "wss" || !strings.EqualFold(u.Host, base.Host) || u.User != nil || u.RawQuery != "" || u.Fragment != "" || u.Path != "/api/remote/device/"+cmd.ID {
		return errors.New("invalid_remote_url")
	}
	// Only the explicitly requested configured local port is bridged. Do not honor
	// additional target_ports or passwordless handoff requests from older platforms.
	if len(cmd.Ports) > 1 || (len(cmd.Ports) == 1 && cmd.Ports[0] != cmd.Port) {
		return errors.New("multiple_ports_unsupported")
	}
	for _, s := range c.Services {
		if s.Port == cmd.Port {
			if (cmd.Service == "terminal" && s.Kind == "terminal") || ((cmd.Service == "router_web" || cmd.Service == "web") && s.Kind == "web") {
				return nil
			}
		}
	}
	return errors.New("port_not_allowed")
}
func (m *bridgeManager) receive(msg mqtt.Message, client mqtt.Client) {
	if msg.Retained() || len(msg.Payload()) > 8192 {
		return
	}
	var cmd remoteCommand
	if json.Unmarshal(msg.Payload(), &cmd) != nil || !topicPart.MatchString(cmd.ID) {
		return
	}
	reject := func(code string) {
		go publish(client, root(m.config)+"/command/result", map[string]any{"request_id": cmd.ID, "status": "rejected", "error": map[string]string{"code": code}})
	}
	if e := validateRemote(m.config, cmd); e != nil {
		reject(e.Error())
		return
	}
	m.mu.Lock()
	for id, until := range m.seen {
		if time.Now().After(until) {
			delete(m.seen, id)
		}
	}
	if _, ok := m.seen[cmd.ID]; ok {
		m.mu.Unlock()
		return
	}
	if len(m.active) >= 4 || len(m.seen) >= 128 {
		m.mu.Unlock()
		reject("session_limit")
		return
	}
	ctx, cancel := context.WithTimeout(m.ctx, time.Duration(cmd.TTL)*time.Second)
	m.active[cmd.ID] = cancel
	m.seen[cmd.ID] = time.Now().Add(12 * time.Hour)
	m.mu.Unlock()
	go func() {
		defer cancel()
		defer func() { m.mu.Lock(); delete(m.active, cmd.ID); m.mu.Unlock() }()
		var workers sync.WaitGroup
		// Four spare TCP streams per session support parallel browser requests/SSE.
		for i := 0; i < 4; i++ {
			workers.Add(1)
			go func() {
				defer workers.Done()
				for ctx.Err() == nil {
					terminal := m.pipe(ctx, cmd)
					if terminal {
						cancel()
						return
					}
					timer := time.NewTimer(time.Second)
					select {
					case <-ctx.Done():
						timer.Stop()
						return
					case <-timer.C:
					}
				}
			}()
		}
		workers.Wait()
	}()
}
func (m *bridgeManager) pipe(ctx context.Context, cmd remoteCommand) bool {
	t, _ := tlsConfig(m.config)
	d := websocket.Dialer{TLSClientConfig: t, HandshakeTimeout: 5 * time.Second, Proxy: nil}
	headers := http.Header{"Authorization": []string{"Bearer " + cmd.Token}, "X-NMS-Target-Port": []string{strconv.Itoa(cmd.Port)}}
	ws, res, e := d.DialContext(ctx, cmd.URL, headers)
	if e != nil {
		if res != nil && res.Body != nil {
			res.Body.Close()
		}
		return res != nil && (res.StatusCode == 401 || res.StatusCode == 403 || res.StatusCode == 410)
	}
	defer ws.Close()
	ws.SetReadLimit(1 << 20)
	tcp, e := (&net.Dialer{Timeout: 3 * time.Second}).DialContext(ctx, "tcp", net.JoinHostPort("127.0.0.1", strconv.Itoa(cmd.Port)))
	if e != nil {
		return true
	}
	defer tcp.Close()
	done := make(chan struct{})
	defer close(done)
	go func() {
		select {
		case <-ctx.Done():
			ws.Close()
			tcp.Close()
		case <-done:
		}
	}()
	copied := make(chan struct{})
	go func() {
		defer close(copied)
		defer ws.Close()
		b := make([]byte, 32768)
		for {
			n, e := tcp.Read(b)
			if n > 0 {
				ws.SetWriteDeadline(time.Now().Add(15 * time.Second))
				if ws.WriteMessage(websocket.BinaryMessage, b[:n]) != nil {
					return
				}
			}
			if e != nil {
				return
			}
		}
	}()
	for {
		kind, r, e := ws.NextReader()
		if e != nil {
			break
		}
		if kind != websocket.BinaryMessage {
			break
		}
		tcp.SetWriteDeadline(time.Now().Add(15 * time.Second))
		if _, e = io.Copy(tcp, r); e != nil {
			break
		}
	}
	tcp.Close()
	ws.Close()
	<-copied
	return false
}
