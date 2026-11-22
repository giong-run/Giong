// gzip-peer: grpc-go with gzip, for the compression that grpc-go's own
// interop programs leave out (they register no compressor and skip the
// *_compressed_* cases).
//
//	gzip-peer -mode=server -port=10001   the interop TestService, gzip registered
//	gzip-peer -mode=client -port=10000   four interop cases, every request gzipped
//
// Run by giong-grpc/interop/run.sh.
package main

import (
	"context"
	"flag"
	"fmt"
	"log"
	"net"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/encoding/gzip"
	"google.golang.org/grpc/interop"
	testgrpc "google.golang.org/grpc/interop/grpc_testing"
)

func main() {
	mode := flag.String("mode", "client", "server or client")
	port := flag.Int("port", 10000, "the port to serve on or to call")
	flag.Parse()
	addr := fmt.Sprintf("127.0.0.1:%d", *port)
	switch *mode {
	case "server":
		lis, err := net.Listen("tcp", addr)
		if err != nil {
			log.Fatalf("listen: %v", err)
		}
		server := grpc.NewServer()
		testgrpc.RegisterTestServiceServer(server, interop.NewTestServer())
		log.Fatal(server.Serve(lis))
	case "client":
		conn, err := grpc.NewClient(addr,
			grpc.WithTransportCredentials(insecure.NewCredentials()),
			grpc.WithDefaultCallOptions(grpc.UseCompressor(gzip.Name)))
		if err != nil {
			log.Fatalf("dial: %v", err)
		}
		defer conn.Close()
		ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
		defer cancel()
		client := testgrpc.NewTestServiceClient(conn)
		// Each of these ends the process on a failure.
		interop.DoLargeUnaryCall(ctx, client)
		fmt.Println("gzip large_unary: ok")
		interop.DoClientStreaming(ctx, client)
		fmt.Println("gzip client_streaming: ok")
		interop.DoServerStreaming(ctx, client)
		fmt.Println("gzip server_streaming: ok")
		interop.DoPingPong(ctx, client)
		fmt.Println("gzip ping_pong: ok")
	default:
		log.Fatalf("unknown mode %q", *mode)
	}
}
