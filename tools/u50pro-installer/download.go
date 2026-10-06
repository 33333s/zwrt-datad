package main

import (
	"context"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"
)

type releaseSpec struct {
	Version, SHA256, BaseURL string
	Bytes                    int64
}

// Populated by the packaging script, together with public CA certificates.
var release releaseSpec
var rootsPEM, serviceScript, transactionScript, startScript, unitFile []byte

func httpsURL(raw string) error {
	u, err := url.Parse(raw)
	if err != nil || u.Scheme != "https" || u.Hostname() == "" || u.User != nil || u.Fragment != "" {
		return fmt.Errorf("expected an HTTPS URL without credentials: %q", raw)
	}
	return nil
}

func downloadClient() *http.Client {
	roots, _ := x509.SystemCertPool()
	if roots == nil {
		roots = x509.NewCertPool()
	}
	roots.AppendCertsFromPEM(rootsPEM)
	dialer := &net.Dialer{Timeout: 15 * time.Second}
	return &http.Client{
		Timeout: 10 * time.Minute,
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, addr string) (net.Conn, error) {
				return dialer.DialContext(ctx, "tcp4", addr)
			},
			TLSClientConfig:     &tls.Config{RootCAs: roots, MinVersion: tls.VersionTLS12},
			TLSHandshakeTimeout: 15 * time.Second, ResponseHeaderTimeout: 30 * time.Second,
		},
		CheckRedirect: func(req *http.Request, via []*http.Request) error {
			if len(via) >= 8 {
				return fmt.Errorf("too many redirects")
			}
			return httpsURL(req.URL.String())
		},
	}
}

type progressWriter struct {
	n, total int64
	last     time.Time
}

func (p *progressWriter) Write(b []byte) (int, error) {
	p.n += int64(len(b))
	if time.Since(p.last) >= time.Second || p.n == p.total {
		fmt.Printf("Download: %d%% (%d / %d bytes)\n", p.n*100/p.total, p.n, p.total)
		p.last = time.Now()
	}
	return len(b), nil
}

func validateELF(path string) error {
	f, err := os.Open(path)
	if err != nil {
		return err
	}
	defer f.Close()
	h := make([]byte, 20)
	if _, err = io.ReadFull(f, h); err != nil {
		return err
	}
	if string(h[:6]) != "\x7fELF\x01\x01" || h[18] != 40 || h[19] != 0 {
		return fmt.Errorf("not an ELF32 little-endian ARM executable")
	}
	return nil
}

func copyVerified(src io.Reader, path string, spec releaseSpec, progress bool) (err error) {
	if spec.Bytes < 64 || spec.Bytes > 64<<20 || len(spec.SHA256) != 64 {
		return fmt.Errorf("invalid embedded release metadata")
	}
	if _, e := os.Lstat(path); !os.IsNotExist(e) {
		return fmt.Errorf("destination already exists or is inaccessible: %s", path)
	}
	part := path + ".part"
	f, err := os.OpenFile(part, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if err != nil {
		return err
	}
	defer func() {
		f.Close()
		if err != nil {
			os.Remove(part)
		}
	}()
	hash := sha256.New()
	writers := []io.Writer{f, hash}
	if progress {
		writers = append(writers, &progressWriter{total: spec.Bytes})
	}
	n, err := io.Copy(io.MultiWriter(writers...), io.LimitReader(src, spec.Bytes+1))
	if err != nil {
		return err
	}
	if n != spec.Bytes {
		return fmt.Errorf("size mismatch: got %d, expected %d", n, spec.Bytes)
	}
	if fmt.Sprintf("%x", hash.Sum(nil)) != spec.SHA256 {
		return fmt.Errorf("SHA-256 mismatch; installation refused")
	}
	if err = f.Sync(); err != nil {
		return err
	}
	if err = f.Close(); err != nil {
		return err
	}
	if err = validateELF(part); err != nil {
		return err
	}
	return os.Rename(part, path)
}

func fetchOnce(ctx context.Context, client *http.Client, raw, path string, spec releaseSpec) error {
	if err := httpsURL(raw); err != nil {
		return err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, raw, nil)
	if err != nil {
		return err
	}
	resp, err := client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("HTTP %d", resp.StatusCode)
	}
	if resp.ContentLength >= 0 && resp.ContentLength != spec.Bytes {
		return fmt.Errorf("unexpected Content-Length: %d", resp.ContentLength)
	}
	return copyVerified(resp.Body, path, spec, true)
}

func obtain(base, local, path string) error {
	if local != "" {
		f, err := os.Open(local)
		if err != nil {
			return err
		}
		defer f.Close()
		return copyVerified(f, path, release, true)
	}
	client := downloadClient()
	defer client.CloseIdleConnections()
	var err error
	for attempt := 1; attempt <= 3; attempt++ {
		fmt.Printf("Download attempt %d/3\n", attempt)
		err = fetchOnce(context.Background(), client, strings.TrimRight(base, "/")+"/zwrt-datad-armv7", path, release)
		if err == nil {
			return nil
		}
		fmt.Println(err)
		if attempt < 3 {
			time.Sleep(2 * time.Second)
		}
	}
	return err
}
