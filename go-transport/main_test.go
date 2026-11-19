package main

import (
	"compress/gzip"
	"context"
	"crypto/tls"
	"io"
	"net"
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

func TestRelayAcceptEncodingGzipAndDecompress(t *testing.T) {
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if got := r.Header.Get("Accept-Encoding"); got != "gzip" {
			t.Fatalf("expected Accept-Encoding=gzip from native Go transport, got %q", got)
		}
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("Content-Encoding", "gzip")
		w.WriteHeader(http.StatusOK)
		gz := gzip.NewWriter(w)
		_, _ = gz.Write([]byte(`{"status":"ok"}`))
		_ = gz.Close()
	}))
	defer upstream.Close()

	proxy := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		outReq, _ := http.NewRequestWithContext(context.Background(), r.Method, upstream.URL+r.URL.Path, r.Body)
		outReq.Header = r.Header.Clone()
		resp, err := (&http.Client{Transport: &http.Transport{DisableCompression: true}}).Do(outReq)
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
	req := httptest.NewRequest(http.MethodPost, "/relay", strings.NewReader(`{}`))
	req.Header.Set(hdrTargetURL, upstream.URL+"/v1internal:loadCodeAssist")
	req.Header.Set(hdrProxyURL, proxy.URL)
	req.Header.Set(hdrProto, "h1")
	req.Header.Set("Accept-Encoding", "gzip, deflate, br, zstd")
	rec := httptest.NewRecorder()

	handler.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("expected 200, got %d: %s", rec.Code, rec.Body.String())
	}
	if rec.Body.String() != `{"status":"ok"}` {
		t.Fatalf("expected decompressed JSON body, got %q", rec.Body.String())
	}
}

func TestRelayTLSALPNAndProtocolModes(t *testing.T) {
	var seenProto string
	var seenALPN string
	var seenCL int64
	var seenTE []string

	upstream := httptest.NewUnstartedServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seenProto = r.Proto
		if r.TLS != nil {
			seenALPN = r.TLS.NegotiatedProtocol
		}
		seenCL = r.ContentLength
		seenTE = r.TransferEncoding
		_, _ = io.ReadAll(r.Body)
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(`ok`))
	}))
	upstream.TLS = &tls.Config{
		NextProtos: []string{"h2", "http/1.1"},
	}
	upstream.EnableHTTP2 = true
	upstream.StartTLS()
	defer upstream.Close()

	// Minimal HTTP CONNECT tunnel proxy
	proxy := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodConnect {
			http.Error(w, "CONNECT required", http.StatusMethodNotAllowed)
			return
		}
		destConn, err := net.DialTimeout("tcp", r.Host, 5*time.Second)
		if err != nil {
			http.Error(w, err.Error(), http.StatusBadGateway)
			return
		}
		w.WriteHeader(http.StatusOK)
		hijacker, ok := w.(http.Hijacker)
		if !ok {
			destConn.Close()
			return
		}
		clientConn, _, err := hijacker.Hijack()
		if err != nil {
			destConn.Close()
			return
		}
		go func() {
			_, _ = io.Copy(destConn, clientConn)
			destConn.Close()
		}()
		go func() {
			_, _ = io.Copy(clientConn, destConn)
			clientConn.Close()
		}()
	}))
	defer proxy.Close()

	pool := newTransportPool()
	// Allow self-signed cert from httptest TLS server in this test
	c1, err := pool.getClient(proxy.URL, "h1", "")
	if err != nil {
		t.Fatal(err)
	}
	c1.Transport.(*http.Transport).TLSClientConfig.InsecureSkipVerify = true

	c2, err := pool.getClient(proxy.URL, "h2", "")
	if err != nil {
		t.Fatal(err)
	}
	tr2 := c2.Transport.(*http.Transport)
	tr2.TLSClientConfig = upstream.Client().Transport.(*http.Transport).TLSClientConfig.Clone()
	tr2.TLSClientConfig.InsecureSkipVerify = true

	handler := newRelayHandler(pool)

	// 1) Test h1 + chunked (streamGenerateContent)
	req1 := httptest.NewRequest(http.MethodPost, "/relay", strings.NewReader(`{"model":"gemini"}`))
	req1.Header.Set(hdrTargetURL, upstream.URL+"/v1internal:streamGenerateContent?alt=sse")
	req1.Header.Set(hdrProxyURL, proxy.URL)
	req1.Header.Set(hdrProto, "h1")
	req1.Header.Set(hdrChunked, "1")
	rec1 := httptest.NewRecorder()
	handler.ServeHTTP(rec1, req1)
	if rec1.Code != http.StatusOK {
		t.Fatalf("h1 expected 200, got %d: %s", rec1.Code, rec1.Body.String())
	}
	if seenProto != "HTTP/1.1" || seenALPN != "http/1.1" {
		t.Fatalf("expected HTTP/1.1 + http/1.1 ALPN, got proto=%s alpn=%s", seenProto, seenALPN)
	}
	if seenCL != -1 || len(seenTE) == 0 || seenTE[0] != "chunked" {
		t.Fatalf("expected chunked transfer-encoding, got ContentLength=%d TE=%v", seenCL, seenTE)
	}

	// 2) Test h2 (oauth2.googleapis.com/token)
	req2 := httptest.NewRequest(http.MethodPost, "/relay", strings.NewReader(`grant_type=refresh_token`))
	req2.Header.Set(hdrTargetURL, upstream.URL+"/token")
	req2.Header.Set(hdrProxyURL, proxy.URL)
	req2.Header.Set(hdrProto, "h2")
	rec2 := httptest.NewRecorder()
	handler.ServeHTTP(rec2, req2)
	if rec2.Code != http.StatusOK {
		t.Fatalf("h2 expected 200, got %d: %s", rec2.Code, rec2.Body.String())
	}
	if seenProto != "HTTP/2.0" || seenALPN != "h2" {
		t.Fatalf("expected HTTP/2.0 + h2 ALPN, got proto=%s alpn=%s", seenProto, seenALPN)
	}
}
