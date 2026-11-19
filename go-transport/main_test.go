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
	c1, _, err := pool.getClient(proxy.URL, "h1", "acc-tls-test", "")
	if err != nil {
		t.Fatal(err)
	}
	c1.Transport.(*http.Transport).TLSClientConfig.InsecureSkipVerify = true

	c2, _, err := pool.getClient(proxy.URL, "h2", "acc-tls-test", "")
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
	req1.Header.Set(hdrAccountID, "acc-tls-test")
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
	req2.Header.Set(hdrAccountID, "acc-tls-test")
	rec2 := httptest.NewRecorder()
	handler.ServeHTTP(rec2, req2)
	if rec2.Code != http.StatusOK {
		t.Fatalf("h2 expected 200, got %d: %s", rec2.Code, rec2.Body.String())
	}
	if seenProto != "HTTP/2.0" || seenALPN != "h2" {
		t.Fatalf("expected HTTP/2.0 + h2 ALPN, got proto=%s alpn=%s", seenProto, seenALPN)
	}
}

func TestRelayProxyErrorHeaderDistinguishesUpstream502(t *testing.T) {
	handler := newRelayHandler(newTransportPool())

	// Case 1: unreachable proxy -> 502 WITH X-Ag-Proxy-Error: 1
	deadLn, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	deadAddr := deadLn.Addr().String()
	deadLn.Close()

	req1 := httptest.NewRequest(http.MethodPost, "/relay", strings.NewReader(`{}`))
	req1.Header.Set(hdrTargetURL, "http://example.com/v1internal:generateContent")
	req1.Header.Set(hdrProxyURL, "http://"+deadAddr)
	req1.Header.Set(hdrProto, "h1")
	rec1 := httptest.NewRecorder()
	handler.ServeHTTP(rec1, req1)

	if rec1.Code != http.StatusBadGateway {
		t.Fatalf("expected 502 for dead proxy, got %d", rec1.Code)
	}
	if rec1.Header().Get(hdrProxyError) != "1" {
		t.Fatalf("expected %s=1 for dead proxy, got %q", hdrProxyError, rec1.Header().Get(hdrProxyError))
	}

	// Case 2: real upstream HTTP 502 (even attempting to spoof header) -> 502 WITHOUT X-Ag-Proxy-Error
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set(hdrProxyError, "1")
		w.WriteHeader(http.StatusBadGateway)
		_, _ = w.Write([]byte("google upstream 502"))
	}))
	defer upstream.Close()

	proxy := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		outReq, _ := http.NewRequestWithContext(context.Background(), r.Method, upstream.URL+r.URL.Path, r.Body)
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

	req2 := httptest.NewRequest(http.MethodPost, "/relay", strings.NewReader(`{}`))
	req2.Header.Set(hdrTargetURL, upstream.URL+"/v1internal:generateContent")
	req2.Header.Set(hdrProxyURL, proxy.URL)
	req2.Header.Set(hdrProto, "h1")
	rec2 := httptest.NewRecorder()
	handler.ServeHTTP(rec2, req2)

	if rec2.Code != http.StatusBadGateway {
		t.Fatalf("expected 502 from upstream, got %d", rec2.Code)
	}
	if rec2.Header().Get(hdrProxyError) != "" {
		t.Fatalf("expected empty %s for real upstream 502, got %q", hdrProxyError, rec2.Header().Get(hdrProxyError))
	}
}

func TestRelayHeaderTimeoutDoesNotTruncateLongStream(t *testing.T) {
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if strings.Contains(r.URL.Path, "slow-headers") {
			select {
			case <-time.After(600 * time.Millisecond):
				w.WriteHeader(http.StatusOK)
			case <-r.Context().Done():
			}
			return
		}
		// Fast headers, slow multi-chunk SSE body (> header timeout)
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		flusher, _ := w.(http.Flusher)
		_, _ = w.Write([]byte("data: part1\n\n"))
		if flusher != nil {
			flusher.Flush()
		}
		time.Sleep(350 * time.Millisecond)
		_, _ = w.Write([]byte("data: part2\n\n"))
		if flusher != nil {
			flusher.Flush()
		}
	}))
	defer upstream.Close()

	proxy := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		outReq, _ := http.NewRequestWithContext(r.Context(), r.Method, upstream.URL+r.URL.Path, r.Body)
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
		flusher, _ := w.(http.Flusher)
		buf := make([]byte, 1024)
		for {
			n, err := resp.Body.Read(buf)
			if n > 0 {
				_, _ = w.Write(buf[:n])
				if flusher != nil {
					flusher.Flush()
				}
			}
			if err != nil {
				return
			}
		}
	}))
	defer proxy.Close()

	handler := newRelayHandler(newTransportPool())

	// 1) Long SSE body exceeding 200ms header timeout MUST NOT be truncated
	req1 := httptest.NewRequest(http.MethodPost, "/relay", strings.NewReader(`{}`))
	req1.Header.Set(hdrTargetURL, upstream.URL+"/v1internal:streamGenerateContent?alt=sse")
	req1.Header.Set(hdrProxyURL, proxy.URL)
	req1.Header.Set(hdrProto, "h1")
	req1.Header.Set(hdrChunked, "1")
	req1.Header.Set(hdrHeaderTimeoutMs, "200")
	rec1 := httptest.NewRecorder()
	handler.ServeHTTP(rec1, req1)

	if rec1.Code != http.StatusOK {
		t.Fatalf("expected 200 for long SSE stream, got %d: %s", rec1.Code, rec1.Body.String())
	}
	if rec1.Body.String() != "data: part1\n\ndata: part2\n\n" {
		t.Fatalf("stream was truncated or corrupted: %q", rec1.Body.String())
	}

	// 2) Hung upstream before response headers MUST time out with X-Ag-Proxy-Error: 1
	req2 := httptest.NewRequest(http.MethodPost, "/relay", strings.NewReader(`{}`))
	req2.Header.Set(hdrTargetURL, upstream.URL+"/slow-headers")
	req2.Header.Set(hdrProxyURL, proxy.URL)
	req2.Header.Set(hdrProto, "h1")
	req2.Header.Set(hdrHeaderTimeoutMs, "150")
	rec2 := httptest.NewRecorder()
	handler.ServeHTTP(rec2, req2)

	if rec2.Code != http.StatusBadGateway {
		t.Fatalf("expected 502 for header timeout, got %d", rec2.Code)
	}
	if rec2.Header().Get(hdrProxyError) != "1" {
		t.Fatalf("expected %s=1 for header timeout, got %q", hdrProxyError, rec2.Header().Get(hdrProxyError))
	}
}

func TestRelayIsolatesConnectionsByAccountID(t *testing.T) {
	// Mock HTTP proxy that returns the client's remote TCP address directly so we can observe socket reuse
	proxy := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get(hdrAccountID) != "" {
			t.Fatalf("internal header %s leaked to proxy/upstream", hdrAccountID)
		}
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(r.RemoteAddr))
	}))
	defer proxy.Close()

	handler := newRelayHandler(newTransportPool())

	send := func(accountID string) string {
		req := httptest.NewRequest(http.MethodPost, "/relay", strings.NewReader(`{}`))
		req.Header.Set(hdrTargetURL, "http://example.com/v1internal:generateContent")
		req.Header.Set(hdrProxyURL, proxy.URL)
		req.Header.Set(hdrProto, "h1")
		if accountID != "" {
			req.Header.Set(hdrAccountID, accountID)
		}
		rec := httptest.NewRecorder()
		handler.ServeHTTP(rec, req)
		if rec.Code != http.StatusOK {
			t.Fatalf("account %q expected 200, got %d: %s", accountID, rec.Code, rec.Body.String())
		}
		return rec.Body.String()
	}

	addrA1 := send("acc_A")
	addrB1 := send("acc_B")
	addrA2 := send("acc_A")
	addrAnon1 := send("")
	addrAnon2 := send("")

	if addrA1 == addrB1 {
		t.Fatalf("different accounts shared TCP connection: acc_A=%s acc_B=%s", addrA1, addrB1)
	}
	if addrA1 != addrA2 {
		t.Fatalf("same account should reuse keep-alive connection: first=%s second=%s", addrA1, addrA2)
	}
	if addrAnon1 == addrAnon2 {
		t.Fatalf("anonymous requests must not share TCP connections: first=%s second=%s", addrAnon1, addrAnon2)
	}
}
