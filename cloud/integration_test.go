package main

import (
	"bufio"
	"context"
	"crypto/tls"
	"encoding/binary"
	"encoding/json"
	"encoding/pem"
	"github.com/gorilla/websocket"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"testing"
	"time"
)

func readPacket(r *bufio.Reader) (byte, []byte, error) {
	h, e := r.ReadByte()
	if e != nil {
		return 0, nil, e
	}
	n, m := 0, 1
	for i := 0; i < 4; i++ {
		v, e := r.ReadByte()
		if e != nil {
			return 0, nil, e
		}
		n += int(v&127) * m
		if v&128 == 0 {
			b := make([]byte, n)
			_, e = io.ReadFull(r, b)
			return h, b, e
		}
		m *= 128
	}
	return 0, nil, io.ErrUnexpectedEOF
}
func TestMQTTTLSReportAndCancel(t *testing.T) {
	certServer := httptest.NewTLSServer(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {}))
	defer certServer.Close()
	listener, e := tls.Listen("tcp", "127.0.0.1:0", certServer.TLS)
	if e != nil {
		t.Fatal(e)
	}
	defer listener.Close()
	reports := make(chan map[string]any, 16)
	finished := make(chan struct{})
	go func() {
		defer close(finished)
		conn, e := listener.Accept()
		if e != nil {
			return
		}
		defer conn.Close()
		conn.SetDeadline(time.Now().Add(10 * time.Second))
		r := bufio.NewReader(conn)
		for {
			h, b, e := readPacket(r)
			if e != nil {
				return
			}
			switch h >> 4 {
			case 1:
				conn.Write([]byte{0x20, 2, 0, 0})
			case 8:
				conn.Write([]byte{0x90, 3, b[0], b[1], 1})
			case 3:
				n := int(binary.BigEndian.Uint16(b))
				off := 2 + n
				if (h>>1)&3 == 1 {
					conn.Write([]byte{0x40, 2, b[off], b[off+1]})
					off += 2
				}
				var payload map[string]any
				if json.Unmarshal(b[off:], &payload) == nil {
					reports <- payload
				}
			case 12:
				conn.Write([]byte{0xd0, 0})
			case 14:
				return
			}
		}
	}()
	state := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte(`{"device":{"firmware_version":"test"},"system":{"memory":{"used":12}},"password":"do-not-upload"}`))
	}))
	defer state.Close()
	c := validConfig()
	c.Broker = "ssl://" + listener.Addr().String()
	c.CAPEM = string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: certServer.Certificate().Raw}))
	a := &agent{stateURL: state.URL}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	result := make(chan error, 1)
	go func() { result <- a.session(ctx, c) }()
	found := map[string]bool{}
	for len(found) < 3 {
		select {
		case p := <-reports:
			b, _ := json.Marshal(p)
			if strings.Contains(string(b), "do-not-upload") {
				t.Fatal("raw state leaked")
			}
			if p["protocol_version"] != float64(1) {
				t.Fatal(p)
			}
			if p["model"] == "MU5252" {
				found["device"] = true
			}
			if p["online"] == true {
				found["online"] = true
			}
			if _, ok := p["boot_id"]; ok {
				found["system"] = true
			}
		case <-time.After(8 * time.Second):
			t.Fatal("missing telemetry", found)
		}
	}
	cancel()
	select {
	case e := <-result:
		if e != nil {
			t.Fatal(e)
		}
	case <-time.After(8 * time.Second):
		t.Fatal("cancel stalled")
	}
	<-finished
}
func TestWebSocketBridgeRoundtripAndClose(t *testing.T) {
	tcp, e := net.Listen("tcp", "127.0.0.1:0")
	if e != nil {
		t.Fatal(e)
	}
	defer tcp.Close()
	tcpDone := make(chan struct{})
	go func() {
		defer close(tcpDone)
		conn, e := tcp.Accept()
		if e != nil {
			return
		}
		defer conn.Close()
		io.Copy(conn, conn)
	}()
	received := make(chan string, 1)
	server := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer "+strings.Repeat("a", 64) {
			w.WriteHeader(401)
			return
		}
		ws, e := (&websocket.Upgrader{}).Upgrade(w, r, nil)
		if e != nil {
			return
		}
		defer ws.Close()
		ws.SetReadDeadline(time.Now().Add(5 * time.Second))
		ws.WriteMessage(websocket.BinaryMessage, []byte("ufi-test"))
		_, b, e := ws.ReadMessage()
		if e == nil {
			received <- string(b)
		}
	}))
	defer server.Close()
	c := validConfig()
	c.RemoteEnabled = true
	c.PlatformURL = server.URL
	c.CAPEM = string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: server.Certificate().Raw}))
	_, port, _ := net.SplitHostPort(tcp.Addr().String())
	n, _ := strconv.Atoi(port)
	c.Services = []Service{{"fixture", n, "web"}}
	cmd := remoteCommand{Version: 1, ID: "session-123", Action: "remote.open", URL: strings.Replace(server.URL, "https:", "wss:", 1) + "/api/remote/device/session-123", Token: strings.Repeat("a", 64), Service: "router_web", Port: n, TTL: 900}
	if e := validateRemote(c, cmd); e != nil {
		t.Fatal(e)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	m := newBridgeManager(ctx, c)
	done := make(chan struct{})
	go func() { defer close(done); m.pipe(ctx, cmd) }()
	select {
	case s := <-received:
		if s != "ufi-test" {
			t.Fatal(s)
		}
	case <-time.After(7 * time.Second):
		t.Fatal("no bridge echo")
	}
	cancel()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("bridge did not close")
	}
	<-tcpDone
	// Removing the test CA must make the same endpoint untrusted.
	c.CAPEM = ""
	m = newBridgeManager(context.Background(), c)
	m.pipe(context.Background(), cmd)
}
