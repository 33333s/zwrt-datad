package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"crypto/x509"
	"encoding/pem"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func fixture() ([]byte, releaseSpec) {
	b := make([]byte, 128)
	copy(b, []byte("\x7fELF\x01\x01"))
	b[18] = 40
	return b, releaseSpec{Bytes: int64(len(b)), SHA256: fmt.Sprintf("%x", sha256.Sum256(b))}
}

func TestVerifiedCopyRejectsInvalidPayload(t *testing.T) {
	good, spec := fixture()
	for _, tc := range []struct {
		name string
		data []byte
		spec releaseSpec
		ok   bool
	}{
		{"valid", good, spec, true},
		{"short", good[:120], spec, false},
		{"oversize", append(append([]byte{}, good...), 0), spec, false},
		{"wrong_hash", append([]byte("FAIL"), good[4:]...), spec, false},
		{"not_elf", make([]byte, 128), releaseSpec{Bytes: 128, SHA256: fmt.Sprintf("%x", sha256.Sum256(make([]byte, 128)))}, false},
		{"bad_metadata", good, releaseSpec{}, false},
	} {
		t.Run(tc.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "binary")
			err := copyVerified(bytes.NewReader(tc.data), path, tc.spec, false)
			if (err == nil) != tc.ok {
				t.Fatalf("error=%v", err)
			}
			if _, err := os.Stat(path + ".part"); !os.IsNotExist(err) {
				t.Fatal("partial file leaked")
			}
			_, err = os.Stat(path)
			if (err == nil) != tc.ok {
				t.Fatal("unexpected destination state")
			}
		})
	}
}

func TestNeverOverwriteDestination(t *testing.T) {
	b, spec := fixture()
	path := filepath.Join(t.TempDir(), "binary")
	os.WriteFile(path, []byte("old"), 0600)
	if copyVerified(bytes.NewReader(b), path, spec, false) == nil {
		t.Fatal("overwrote existing file")
	}
	got, _ := os.ReadFile(path)
	if string(got) != "old" {
		t.Fatal("old file changed")
	}
}

func TestHTTPSIntegrityAndRedirects(t *testing.T) {
	b, spec := fixture()
	server := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/good":
			w.Write(b)
		case "/redirect":
			http.Redirect(w, r, "/good", http.StatusFound)
		case "/downgrade":
			http.Redirect(w, r, "http://127.0.0.1/unsafe", http.StatusFound)
		case "/loop":
			http.Redirect(w, r, "/loop", http.StatusFound)
		case "/html":
			w.Write([]byte("<html>login</html>"))
		default:
			http.NotFound(w, r)
		}
	}))
	defer server.Close()
	old := rootsPEM
	defer func() { rootsPEM = old }()
	rootsPEM = pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: server.Certificate().Raw})
	// No system certificates: exercise the embedded CA fallback explicitly.
	t.Setenv("SSL_CERT_FILE", filepath.Join(t.TempDir(), "missing"))
	t.Setenv("SSL_CERT_DIR", t.TempDir())
	client := downloadClient()
	defer client.CloseIdleConnections()
	for _, path := range []string{"/good", "/redirect", "/downgrade", "/loop", "/html", "/404"} {
		t.Run(path, func(t *testing.T) {
			err := fetchOnce(context.Background(), client, server.URL+path, filepath.Join(t.TempDir(), "binary"), spec)
			want := path == "/good" || path == "/redirect"
			if (err == nil) != want {
				t.Fatalf("error=%v", err)
			}
		})
	}
	transport := client.Transport.(*http.Transport)
	transport.CloseIdleConnections()
	transport.TLSClientConfig.RootCAs = x509.NewCertPool()
	if fetchOnce(context.Background(), client, server.URL+"/good", filepath.Join(t.TempDir(), "binary"), spec) == nil {
		t.Fatal("untrusted TLS certificate accepted")
	}
}

func TestHTTPSOnly(t *testing.T) {
	for _, raw := range []string{"http://host/path", "https://user:pass@host/", "file:///tmp/file", "https:///missing-host"} {
		if httpsURL(raw) == nil {
			t.Errorf("accepted %s", raw)
		}
	}
	if err := httpsURL("https://example.com/a%20b"); err != nil {
		t.Fatal(err)
	}
}

func TestOfflineCopy(t *testing.T) {
	b, spec := fixture()
	old := release
	release = spec
	defer func() { release = old }()
	source := filepath.Join(t.TempDir(), "local binary")
	os.WriteFile(source, b, 0600)
	target := filepath.Join(t.TempDir(), "copy")
	if err := obtain("", source, target); err != nil {
		t.Fatal(err)
	}
	os.WriteFile(source, []byte(strings.Repeat("x", 128)), 0600)
	if err := obtain("", source, filepath.Join(t.TempDir(), "bad")); err == nil {
		t.Fatal("accepted corrupt local binary")
	}
}
