#!/bin/sh
# Build nginx statically against musl for Talaria (and for the Linux side of the
# comparison — the binary is one and the same, as with tlgen).
#
# What is disabled and why:
#   rewrite/PCRE, gzip/zlib, ssl/OpenSSL — third-party libraries unrelated to what
#     is measured (epoll + sockets + serving static files);
#   http_proxy/fastcgi/uwsgi/scgi/memcached — the test case has no upstreams;
#   sendfile — Talaria does not promise it, and on Linux it would give an advantage
#     the other side does not have: both sides serve files through write.
# One worker thread, master_process off — one process, like the other servers of
# the suite.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
V=${V:-1.24.0}
SRC="$HERE/build/nginx-$V"
[ -d "$SRC" ] || { echo "no sources in $SRC — see README"; exit 1; }

docker run --rm -v "$HERE":/w -w "/w/build/nginx-$V" talaria-musl sh -eu -c '
./configure \
  --prefix=/nginx \
  --sbin-path=/nginx/nginx \
  --conf-path=/nginx/nginx.conf \
  --pid-path=/nginx/nginx.pid \
  --error-log-path=/nginx/error.log \
  --http-log-path=/nginx/access.log \
  --http-client-body-temp-path=/nginx/tmp/body \
  --http-proxy-temp-path=/nginx/tmp/proxy \
  --http-fastcgi-temp-path=/nginx/tmp/fastcgi \
  --http-uwsgi-temp-path=/nginx/tmp/uwsgi \
  --http-scgi-temp-path=/nginx/tmp/scgi \
  --without-http_rewrite_module \
  --without-http_gzip_module \
  --without-http_proxy_module \
  --without-http_fastcgi_module \
  --without-http_uwsgi_module \
  --without-http_scgi_module \
  --without-http_memcached_module \
  --without-http_ssi_module \
  --without-http_userid_module \
  --without-http_auth_basic_module \
  --without-http_autoindex_module \
  --without-http_geo_module \
  --without-http_map_module \
  --without-http_split_clients_module \
  --without-http_referer_module \
  --without-http_upstream_hash_module \
  --without-http_upstream_ip_hash_module \
  --without-http_upstream_least_conn_module \
  --without-http_upstream_random_module \
  --without-http_upstream_keepalive_module \
  --without-http_upstream_zone_module \
  --without-mail_pop3_module --without-mail_imap_module --without-mail_smtp_module \
  --without-stream_access_module --without-stream_geo_module \
  --without-stream_limit_conn_module --without-stream_map_module \
  --without-stream_return_module --without-stream_split_clients_module \
  --without-stream_upstream_hash_module --without-stream_upstream_least_conn_module \
  --without-stream_upstream_random_module --without-stream_upstream_zone_module \
  --without-pcre --without-pcre2 \
  --with-cc=musl-gcc \
  --with-cc-opt="-O2 -D_GNU_SOURCE" \
  --with-ld-opt="-static -no-pie -Wl,-Ttext-segment=0x10000000"
make -j"$(nproc)"
' 2>&1 | tail -25

cp "$SRC/objs/nginx" "$HERE/build/nginx"
file "$HERE/build/nginx"
echo "done: services/nginx/build/nginx"
