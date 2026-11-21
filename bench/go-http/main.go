// The benchmark routes on Go's net/http, for bench/memory.sh and bench/small-box.sh.
package main

import (
	"io"
	"net/http"
	"os"
	"strconv"
	"time"
)

func main() {
	mux := http.NewServeMux()
	mux.HandleFunc("/json", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"message":"Hello, World!"}`))
	})
	// /wait?ms=N answers after N milliseconds, like a handler waiting for
	// a database or another service.
	mux.HandleFunc("/wait", func(w http.ResponseWriter, r *http.Request) {
		ms, _ := strconv.Atoi(r.URL.Query().Get("ms"))
		time.Sleep(time.Duration(ms) * time.Millisecond)
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"message":"Hello, World!"}`))
	})
	// POST /upload reads the whole body before answering, as a handler that
	// takes a JSON or form body does (bench/small-box.sh).
	mux.HandleFunc("/upload", func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			w.WriteHeader(http.StatusBadRequest)
			return
		}
		w.Write([]byte(strconv.Itoa(len(body))))
	})
	host := os.Getenv("BENCH_HOST")
	if host == "" {
		host = "127.0.0.1"
	}
	// With TLS_CERT and TLS_KEY (PEM files): HTTPS on port 3012, HTTP/2
	// chosen through ALPN as net/http does by default (bench/small-box.sh).
	if cert, key := os.Getenv("TLS_CERT"), os.Getenv("TLS_KEY"); cert != "" && key != "" {
		srv := &http.Server{Addr: host + ":3012", Handler: mux}
		if err := srv.ListenAndServeTLS(cert, key); err != nil {
			panic(err)
		}
		return
	}
	srv := &http.Server{Addr: host + ":3011", Handler: mux}
	if err := srv.ListenAndServe(); err != nil {
		panic(err)
	}
}
