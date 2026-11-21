# AF_XDP against kernel sockets, giong and Tokio: small UDP echo

`sudo bench/xdp.sh 5 8`. `vm/`: 4-vCPU cloud VM (Linux 6.18), veth between two network
namespaces, XDP in driver mode on veth, AF_XDP in copy mode. Columns: echoes per second;
CPU of the whole machine per echo (client included); the server process's CPU per echo;
softirq time on the server's CPU per echo. Method and caveats:
`docs/reviews/2026-11-speed-ceiling.md`, section 2.1.
