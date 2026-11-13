// The benchmark routes on Go's net/http, for bench/memory.sh.
package main

import (
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
	host := os.Getenv("BENCH_HOST")
	if host == "" {
		host = "127.0.0.1"
	}
	srv := &http.Server{Addr: host + ":3011", Handler: mux}
	if err := srv.ListenAndServe(); err != nil {
		panic(err)
	}
}
