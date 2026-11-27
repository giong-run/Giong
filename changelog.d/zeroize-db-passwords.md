### Security

- **giong-postgres**, **giong-mysql**, **giong-redis**: each `Config` wipes its copy of the
  password from memory when it is dropped, and so is every copy the client makes while
  logging in: the decoded URL, the packet or command that carries it (wiped once written),
  and what is derived from it (the SCRAM salted password and keys, including the cached
  ones, the MD5 hash PostgreSQL stores, MySQL's RSA-encrypted login with its padding). Each
  of those buffers is sized so that it never grows, which would leave a copy behind in
  freed memory. A memory dump, or a bug that reads freed memory, no longer finds the
  password there; copies outside the client (the string the application passes in, the
  kernel's socket buffers) are not in its hands. Running queries does not change.
