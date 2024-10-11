# `wasi-http` test harness

This repo is for testing experimental
[wasi-http](https://github.com/WebAssembly/wasi-http) features such as
`outgoing-body.append`.

## Building and running

### Prerequisite(s)

- Recent Rust nightly, including the `wasm32-wasip2` target

```shell
(cd host && cargo +nightly test)
```
