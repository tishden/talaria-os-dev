# nginx on Talaria

nginx 1.24.0, **sources unchanged**: a static musl build linked at `0x10000000`
(the Talaria user region), one process, one worker thread.

## Build

```sh
mkdir -p services/nginx/build && cd services/nginx/build
curl -sLO https://nginx.org/download/nginx-1.24.0.tar.gz && tar xzf nginx-1.24.0.tar.gz
cd ../../.. && services/nginx/build.sh          # -> services/nginx/build/nginx
```

What is disabled in the build and why is described in the header of
`build.sh`. In short: third-party libraries (PCRE, zlib, OpenSSL) have nothing
to do with what is measured, and `sendfile` is turned off in the configuration
on purpose — Talaria does not have it, so both sides of the comparison serve
files through `write`.

A consequence of building without PCRE: there is no `rewrite` module and hence
no `return` directive — the "from memory, without the file system" response is
served by `empty_gif` (43 bytes from a static array).

## Test cases

| path | what it loads |
|---|---|
| `/nop` | the pure network path: accept, parse the request, answer from memory |
| `/` | the same plus the file system: `openat`/`fstat`/`read`/`close` per request |

Both are measured with `bench/suite/tlgen --proto http`.

## Running

On the host (to check the build):
```sh
services/nginx/build/nginx -p services/nginx/build/root/nginx/ -c nginx.conf
curl -s localhost:8080/nop
```

On Talaria: `services/nginx/mkdisk.sh` builds an ext2 image with the `/nginx`
tree (binary, `nginx.conf`, `html/`, empty `tmp/`, `error.log`) and the
`/cell.manifest`; `services/nginx/run.sh` boots the kernel with that image and
a tap network.
