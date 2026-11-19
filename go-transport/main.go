package main

import (
	"context"
	"crypto/tls"
	"flag"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/url"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"
)

const (
	hdrTargetURL = "X-Ag-Target-Url"
	hdrProxyURL  = "X-Ag-Proxy"
	hdrProto     = "X-Ag-Proto"
	hdrChunked   = "X-Ag-Chunked"
	hdrTimeoutMs = "X-Ag-Timeout-Ms"
	hdrEvictGen  = "X-Ag-Evict-Gen"
)

type transportEntry struct {
	client    *http.Client
	transport *http.Transport
	evictGen  string
}

type transportPool struct {
	mu      sync.RWMutex
	entries map[string]*transportEntry
}

func newTransportPool() *transportPool {
	return &transportPool{
		entries: make(map[string]*transportEntry),
	}
}

func normalizeProxyURL(raw string) (*url.URL, error) {
	trimmed := strings.TrimSpace(raw)
	if trimmed == "" {
		return nil, fmt.Errorf("empty proxy url")
	}
	// Go net/http.Transport natively dials SOCKS5 with domain resolution when scheme is "socks5"
	if strings.HasPrefix(strings.ToLower(trimmed), "socks5h://") {
		trimmed = "socks5://" + trimmed[len("socks5h://"):]
	}
	u, err := url.Parse(trimmed)
	if err != nil {
		return nil, err
	}
	if u.Scheme == "" || u.Host == "" {
		return nil, fmt.Errorf("invalid proxy url: %s", trimmed)
	}
	return u, nil
}

func buildTransport(proxyURL *url.URL, proto string) *http.Transport {
	dialer := &net.Dialer{
		Timeout:   20 * time.Second,
		KeepAlive: 60 * time.Second,
	}
	tr := &http.Transport{
		Proxy:                 http.ProxyURL(proxyURL),
		DialContext:           dialer.DialContext,
		MaxIdleConns:          100,
		MaxIdleConnsPerHost:   20,
		IdleConnTimeout:       90 * time.Second,
		TLSHandshakeTimeout:   20 * time.Second,
		ExpectContinueTimeout: 1 * time.Second,
		DisableCompression:    true,
	}
	if proto == "h2" {
		// Native Go HTTP/2 (matches Go-http-client/2.0 against oauth2.googleapis.com)
		tr.ForceAttemptHTTP2 = true
	} else {
		// Force HTTP/1.1 ALPN + disable HTTP/2 roundtripper (matches aidev_client 100% HTTP/1.1 captures)
		tr.ForceAttemptHTTP2 = false
		tr.TLSClientConfig = &tls.Config{
			NextProtos: []string{"http/1.1"},
		}
		tr.TLSNextProto = make(map[string]func(authority string, c *tls.Conn) http.RoundTripper)
	}
	return tr
}

func (p *transportPool) getClient(proxyRaw, proto, evictGen string) (*http.Client, error) {
	if proto != "h2" {
		proto = "h1"
	}
	proxyURL, err := normalizeProxyURL(proxyRaw)
	if err != nil {
		return nil, err
	}
	key := proto + "|" + proxyURL.String()

	p.mu.RLock()
	if ent, ok := p.entries[key]; ok && ent.evictGen == evictGen {
		p.mu.RUnlock()
		return ent.client, nil
	}
	p.mu.RUnlock()

	p.mu.Lock()
	defer p.mu.Unlock()
	if ent, ok := p.entries[key]; ok {
		if ent.evictGen == evictGen {
			return ent.client, nil
		}
		ent.transport.CloseIdleConnections()
	}

	tr := buildTransport(proxyURL, proto)
	client := &http.Client{
		Transport: tr,
	}
	p.entries[key] = &transportEntry{
		client:    client,
		transport: tr,
		evictGen:  evictGen,
	}
	return client, nil
}

func isInternalHeader(name string) bool {
	return strings.EqualFold(name, hdrTargetURL) ||
		strings.EqualFold(name, hdrProxyURL) ||
		strings.EqualFold(name, hdrProto) ||
		strings.EqualFold(name, hdrChunked) ||
		strings.EqualFold(name, hdrTimeoutMs) ||
		strings.EqualFold(name, hdrEvictGen)
}

func isHopByHopHeader(name string) bool {
	switch strings.ToLower(name) {
	case "connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailers", "transfer-encoding", "upgrade", "host":
		return true
	}
	return false
}

func newRelayHandler(pool *transportPool) http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/health", func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("ok"))
	})
	mux.HandleFunc("/relay", func(w http.ResponseWriter, r *http.Request) {
		targetRaw := r.Header.Get(hdrTargetURL)
		proxyRaw := r.Header.Get(hdrProxyURL)
		proto := strings.ToLower(strings.TrimSpace(r.Header.Get(hdrProto)))
		chunked := r.Header.Get(hdrChunked) == "1"
		timeoutMsStr := r.Header.Get(hdrTimeoutMs)
		evictGen := r.Header.Get(hdrEvictGen)

		if targetRaw == "" {
			http.Error(w, "missing "+hdrTargetURL, http.StatusBadRequest)
			return
		}
		targetURL, err := url.Parse(targetRaw)
		if err != nil || (targetURL.Scheme != "https" && targetURL.Scheme != "http") {
			http.Error(w, "invalid "+hdrTargetURL, http.StatusBadRequest)
			return
		}

		// Strict proxy policy: refuse direct egress if proxy header is missing/invalid
		client, err := pool.getClient(proxyRaw, proto, evictGen)
		if err != nil {
			http.Error(w, "strict proxy error: "+err.Error(), http.StatusBadGateway)
			return
		}

		ctx := r.Context()
		var cancel func()
		if timeoutMsStr != "" {
			if ms, err := strconv.Atoi(timeoutMsStr); err == nil && ms > 0 {
				ctx, cancel = context.WithTimeout(ctx, time.Duration(ms)*time.Millisecond)
				defer cancel()
			}
		}

		var body io.Reader
		if r.Body != nil && r.Method != http.MethodGet && r.Method != http.MethodHead {
			body = r.Body
		}
		outReq, err := http.NewRequestWithContext(ctx, r.Method, targetURL.String(), body)
		if err != nil {
			http.Error(w, "build upstream request failed: "+err.Error(), http.StatusBadRequest)
			return
		}

		for k, vals := range r.Header {
			if isInternalHeader(k) || isHopByHopHeader(k) {
				continue
			}
			for _, v := range vals {
				outReq.Header.Add(k, v)
			}
		}
		if chunked {
			outReq.ContentLength = -1
		} else if r.ContentLength >= 0 {
			outReq.ContentLength = r.ContentLength
		}

		resp, err := client.Do(outReq)
		if err != nil {
			http.Error(w, "upstream transport error: "+err.Error(), http.StatusBadGateway)
			return
		}
		defer resp.Body.Close()

		for k, vals := range resp.Header {
			if isHopByHopHeader(k) {
				continue
			}
			for _, v := range vals {
				w.Header().Add(k, v)
			}
		}
		w.WriteHeader(resp.StatusCode)

		flusher, canFlush := w.(http.Flusher)
		buf := make([]byte, 32*1024)
		for {
			n, readErr := resp.Body.Read(buf)
			if n > 0 {
				if _, writeErr := w.Write(buf[:n]); writeErr != nil {
					return
				}
				if canFlush {
					flusher.Flush()
				}
			}
			if readErr != nil {
				return
			}
		}
	})
	return mux
}

func main() {
	listenAddr := flag.String("listen", "127.0.0.1:0", "loopback listen address")
	watchStdin := flag.Bool("watch-stdin", true, "exit when stdin closes")
	flag.Parse()

	if *watchStdin {
		go func() {
			_, _ = io.Copy(io.Discard, os.Stdin)
			os.Exit(0)
		}()
	}

	ln, err := net.Listen("tcp", *listenAddr)
	if err != nil {
		log.Fatalf("listen %s failed: %v", *listenAddr, err)
	}

	// Print ready address on first stdout line for parent process discovery
	fmt.Printf("READY %s\n", ln.Addr().String())
	_ = os.Stdout.Sync()

	srv := &http.Server{
		Handler:           newRelayHandler(newTransportPool()),
		ReadHeaderTimeout: 15 * time.Second,
	}
	if err := srv.Serve(ln); err != nil && err != http.ErrServerClosed {
		log.Fatalf("serve failed: %v", err)
	}
}
