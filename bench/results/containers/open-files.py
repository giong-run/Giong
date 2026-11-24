# Used by open-files.sh: open-files.py <port> <connections>
import socket, sys, time
port=int(sys.argv[1]); n=int(sys.argv[2])
conns=[]; served=0
for i in range(n):
    s=socket.create_connection(('127.0.0.1',port),2); conns.append(s)
for s in conns:
    s.settimeout(2)
    try:
        s.sendall(b'GET / HTTP/1.1\r\nHost: x\r\n\r\n'); d=s.recv(100)
        if d.startswith(b'HTTP/1.1 200'): served+=1
    except Exception: pass
print(f"held {n} connections, {served} answered within 2 s each")
t=time.time()
try:
    s=socket.create_connection(('127.0.0.1',port),2); s.settimeout(5); s.sendall(b'GET / HTTP/1.1\r\nHost: x\r\n\r\n'); d=s.recv(100); print("new client:", d[:15], f"{time.time()-t:.2f}s")
except Exception as e: print("new client failed:", e, f"{time.time()-t:.2f}s")
