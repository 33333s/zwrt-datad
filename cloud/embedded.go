package main

/*
#include <stdlib.h>
*/
import "C"

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
	"unsafe"
)

type embeddedResponse struct {
	header http.Header
	body   bytes.Buffer
	status int
}

func (w *embeddedResponse) Header() http.Header { return w.header }
func (w *embeddedResponse) WriteHeader(code int) {
	if w.status == 0 {
		w.status = code
	}
}
func (w *embeddedResponse) Write(b []byte) (int, error) {
	if w.status == 0 {
		w.status = http.StatusOK
	}
	return w.body.Write(b)
}

var embedded struct {
	sync.Mutex
	agent  *agent
	ota    *otaManager
	cancel context.CancelFunc
	done   chan struct{}
}

func cString(p *C.char) string {
	if p == nil {
		return ""
	}
	return C.GoString(p)
}

//export CloudStart
func CloudStart(dirC, stateURLC *C.char) C.int {
	dir, stateURL := cString(dirC), cString(stateURLC)
	if dir == "" || stateURL == "" {
		return 1
	}
	if err := os.MkdirAll(dir, 0700); err != nil {
		return 1
	}

	embedded.Lock()
	defer embedded.Unlock()
	if embedded.agent != nil {
		return 0
	}
	a := &agent{
		config: defaults(), file: filepath.Join(dir, "cloud.json"), stateURL: stateURL,
		change: make(chan struct{}, 1), status: status{State: "disabled"},
	}
	o, err := newOTAManager(dir, stateURL)
	if err != nil {
		return 1
	}
	if b, err := os.ReadFile(a.file); err == nil {
		var saved Config
		if json.Unmarshal(b, &saved) != nil || validate(saved) != nil {
			a.status = status{State: "error", Error: "云端配置无效，请重新保存"}
			a.loadError = true
		} else {
			a.config = saved
			if saved.Enabled {
				a.status.State = "starting"
			}
		}
	} else if !os.IsNotExist(err) {
		a.status = status{State: "error", Error: "无法读取云端配置"}
		a.loadError = true
	}
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	embedded.agent, embedded.ota, embedded.cancel, embedded.done = a, o, cancel, done
	go func() { defer close(done); go o.run(ctx); a.run(ctx) }()
	return 0
}

//export CloudStop
func CloudStop() {
	embedded.Lock()
	cancel, done := embedded.cancel, embedded.done
	embedded.agent, embedded.ota, embedded.cancel, embedded.done = nil, nil, nil, nil
	embedded.Unlock()
	if cancel == nil {
		return
	}
	cancel()
	select {
	case <-done:
	case <-time.After(8 * time.Second):
	}
}

//export CloudHandle
func CloudHandle(methodC, pathC, bodyC *C.char, statusC *C.int) *C.char {
	embedded.Lock()
	a := embedded.agent
	o := embedded.ota
	embedded.Unlock()
	if a == nil {
		if statusC != nil {
			*statusC = http.StatusServiceUnavailable
		}
		return C.CString("{\"error\":\"datad cloud runtime unavailable\"}\n")
	}
	method, path, body := cString(methodC), cString(pathC), cString(bodyC)
	req, err := http.NewRequest(method, "http://127.0.0.1"+path, strings.NewReader(body))
	if err != nil {
		if statusC != nil {
			*statusC = http.StatusBadRequest
		}
		return C.CString("{\"error\":\"配置请求无效\"}\n")
	}
	w := &embeddedResponse{header: make(http.Header)}
	if strings.HasPrefix(path, "/ota/") && o != nil {
		switch path {
		case "/ota/config":
			o.configAPI(w, req)
		case "/ota/status":
			o.statusAPI(w, req)
		case "/ota/check":
			o.checkAPI(w, req)
		case "/ota/update":
			o.updateAPI(w, req)
		default:
			w.WriteHeader(http.StatusNotFound)
			_, _ = w.Write([]byte("{\"error\":\"not found\"}\n"))
		}
	} else if path == "/cloud/config" {
		a.configAPI(w, req)
	} else if path == "/cloud/status" {
		a.statusAPI(w, req)
	} else {
		w.WriteHeader(http.StatusNotFound)
		_, _ = w.Write([]byte("{\"error\":\"not found\"}\n"))
	}
	if w.status == 0 {
		w.status = http.StatusOK
	}
	if statusC != nil {
		*statusC = C.int(w.status)
	}
	return C.CString(w.body.String())
}

//export CloudFree
func CloudFree(p *C.char) { C.free(unsafe.Pointer(p)) }
