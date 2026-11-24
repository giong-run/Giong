### Changed

- **giong-http**: an HTTP/3 request costs 6.6% fewer CPU instructions, client and server
  counted together (114,971 to 107,418 for a small GET on one worker over loopback). QPACK
  finds the static entries of a field name with one `match` instead of a binary search that
  compared about a dozen names per field, and the Huffman code of HPACK and QPACK encodes
  32 bits at a time into the bytes it reserved, computing the coded length once instead of
  twice (39% fewer instructions), and decodes without a branch per four bits (14% fewer).
