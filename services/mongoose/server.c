#include "mongoose.h"

// HTTP: a static file from disk (/), a dynamic answer for other paths.
static void fn(struct mg_connection *c, int ev, void *ev_data) {
  if (ev == MG_EV_HTTP_MSG) {
    struct mg_http_message *hm = (struct mg_http_message *) ev_data;
    if (mg_strcmp(hm->uri, mg_str("/")) == 0 ||
        mg_strcmp(hm->uri, mg_str("/index.html")) == 0) {
      struct mg_http_serve_opts opts; memset(&opts, 0, sizeof(opts));
      opts.mime_types = "html=text/html";
      mg_http_serve_file(c, hm, "index.html", &opts); // streamed from the TFS1 disk
    } else {
      struct mg_str *host = mg_http_get_header(hm, "Host");
      mg_http_reply(c, 200, "Content-Type: text/plain; charset=utf-8\r\n",
        "Dynamic answer from mongoose %s on Talaria OS\nMethod: %.*s  URI: %.*s  Host: %.*s\n",
        MG_VERSION, (int) hm->method.len, hm->method.buf,
        (int) hm->uri.len, hm->uri.buf, host ? (int) host->len : 0, host ? host->buf : "");
    }
  }
}

int main(void) {
  struct mg_mgr mgr;
  mg_mgr_init(&mgr);
  mg_http_listen(&mgr, "http://0.0.0.0:80", fn, NULL);
  for (;;) mg_mgr_poll(&mgr, 100);
  mg_mgr_free(&mgr);
  return 0;
}
