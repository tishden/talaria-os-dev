# smoltcp 0.14.0, vendored

The crate is `smoltcp` 0.14.0 from crates.io (0BSD, `LICENSE-0BSD.txt`), with its `src/`,
`build.rs`, `gen_config.py`, `Cargo.toml`, `README.md` and `CHANGELOG.md`; the tests, benches,
examples and CI files are left out. One change against the published crate:

* `src/socket/tcp.rs`: `RTTE_MIN_RTO` 1000 -> 200 ms. The RFC 6298 floor of one second made
  every lost segment on a closed-loop request/response connection (no duplicate ACKs to
  recover by) cost exactly 1.000 s + one RTT: the Talaria load generator's ladder on a
  `c7g.xlarge` showed a max of 1 000 067..1 000 124 us at every rung from 77 k req/s up while
  the Linux generator's max stayed at 300 us (`docs/ARM64-PLAN.md` §11 item 2a). Linux's floor
  is 200 ms (`TCP_RTO_MIN`); the gate `txloss` on both machines drops every N-th transmitted
  frame (`talaria.txdrop=N`) and asserts a round trip over a drop costs under 600 ms.

To move to a newer smoltcp, re-apply that one constant (or whatever knob replaces it).
