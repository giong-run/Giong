# Soak with balanced listeners

`tests/soak.rs` binds balanced listeners on its even-numbered workers, so connections are
also handed from worker to worker.

- `epoll-cc6aff2.log.gz`: `GIONG_DRIVER=epoll GIONG_SOAK_SECS=6900 cargo test --release
  --test soak -- --ignored --nocapture` on commit cc6aff2 (the rule before the count step
  for saturated workers), Linux 6.18, 4 vCPUs: 151,560,037 connections, 10,266,939
  cross-worker hops, descriptors and memory flat, 0 unexpected errors.
- `epoll-bbf13c8.log.gz`: the same on commit bbf13c8 (the final rule): 151,506,771
  connections, 10,243,293 cross-worker hops, descriptors and memory flat, 0 unexpected
  errors.
- The Soak workflow, 240 minutes, on commit 12a264f
  ([run 44883087572](https://github.com/giong-run/Giong/actions/runs/44883087572)): all five
  jobs passed (runner kernel: 317,381,629 connections; Linux 6.18 in QEMU: 829,766,373).
- The Soak workflow, 240 minutes, on commit bbf13c8 (the final rule)
  ([run 44903037710](https://github.com/giong-run/Giong/actions/runs/44903037710)): all five
  jobs passed (runner kernel: 317,285,649 connections, 28,610,288 cross-worker hops;
  Linux 6.18 in QEMU: 828,640,512 connections), 0 connects refused.
