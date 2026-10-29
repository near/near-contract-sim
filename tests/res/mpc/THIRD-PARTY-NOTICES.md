# Third-party notices for the mpc fixtures

`mpc_contract.wasm`, `mpc_contract_8397c0ae.wasm` and `tee_verifier.wasm`
are built from https://github.com/near/mpc and statically link its
third-party Rust dependencies. near/mpc keeps their license texts and
copyright notices, generated with `cargo about`, in its
`third-party-licenses/` directory (`licenses.html`, about 1.1 MB, too large
to copy here):

- https://github.com/near/mpc/tree/1d514bc4001028b8c4a8af0190d84167216aa818/third-party-licenses
  for `mpc_contract.wasm` and `tee_verifier.wasm`;
- https://github.com/near/mpc/tree/8397c0aeb67a207f28f0f5d757e304eea993f018/third-party-licenses
  for `mpc_contract_8397c0ae.wasm`.

near/mpc itself is MIT licensed: see [LICENSE](LICENSE).
