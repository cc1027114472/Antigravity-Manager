package main

import (
	"context"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func TestStrictProxyRejectsMissingProxy(t *testing.T) {
	handler := newRelayHandler(newTransportPool())
	req := httptest.NewRequest(http.MethodPost, "/relay", strings.NewReader(`{}`))
	req.Header.Set(hdrTargetURL, "https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist")
	rec := httptest.NewRecorder()

	handler.ServeHTTP(rec, req)
	if rec.Code != http.StatusBadGateway {
		t.Fatalf("expected 502 when X-Ag-Proxy missing, got %d", rec.Code)
	}
}

func TestRelayStreamsThroughHttpProxy(t *testing.T) {
	// Mock upstream HTTP server
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get(hdrTargetURL) != "" || r.Header.Get(hdrProxyURL) != "" {
			t.Fatalf("internal headers leaked to upstream: %v", r.Header)
		}
		if r.Header.Get("User-Agent") != "antigravity/hub/2.31.0" {
			t.Fatalf("unexpected User-Agent: %s", r.Header.Get("User-Agent"))
		}
		body, _ := io.ReadAll(r.Body)
		if string(body) != `{"hello":"world"}` {
			t.Fatalf("unexpected body: %s", string(body))
		}
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("data: ok\n\n"))
	}))
	defer upstream.Close()

	// Mock HTTP forward proxy
	proxy := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		outReq, _ := http.NewRequestWithContext(context.Background(), r.Method, upstream.URL+r.URL.Path, r.Body)
		outReq.Header = r.Header.Clone()
		resp, err := http.DefaultClient.Do(outReq)
		if err != nil {
			http.Error(w, err.Error(), http.StatusBadGateway)
			return
		}
		defer resp.Body.Close()
		for k, vs := range resp.Header {
			for _, v := range vs {
				w.Header().Add(k, v)
			}
		}
		w.WriteHeader(resp.StatusCode)
		_, _ = io.Copy(w, resp.Body)
	}))
	defer proxy.Close()

	handler := newRelayHandler(newTransportPool())
	req := httptest.NewRequest(http.MethodPost, "/relay", strings.NewReader(`{"hello":"world"}`))
	req.Header.Set(hdrTargetURL, upstream.URL+"/v1internal:streamGenerateContent?alt=sse")
	req.Header.Set(hdrProxyURL, proxy.URL)
	req.Header.Set(hdrProto, "h1")
	req.Header.Set(hdrChunked, "1")
	req.Header.Set("User-Agent", "antigravity/hub/2.31.0")
	req.Header.Set("Content-Type", "application/json")
	rec := httptest.NewRecorder()

	ctx, cancel := context.WithTimeout(req.Context(), 5*time.Second)
	defer cancel()
	handler.ServeHTTP(rec, req.WithContext(ctx))

	if rec.Code != http.StatusOK {
		t.Fatalf("expected 200, got %d: %s", rec.Code, rec.Body.String())
	}
	if rec.Body.String() != "data: ok\n\n" {
		t.Fatalf("unexpected streamed body: %q", rec.Body.String())
	}
}
