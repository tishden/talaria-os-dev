// Talaria zero-syscall backend: frames go through two SPSC rings in memory shared
// with the Talaria kernel's datapath cell (kernel/src/fastpath.rs, fastapp.c).
// The cell (a dedicated core, ring 0) bridges virtio-net <-> rings; this process
// never makes a syscall on the hot path. Raw Ethernet/IPv4/UDP frames built with
// io/pkt.h (like the dpdk/xdp backends); ARP for our IP is answered by the cell.
// Peer MAC is static (--dst-mac); our MAC from TALARIA_MAC env, our IP --dpdk-ip.
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <string>
#include <arpa/inet.h>
#include "backend.h"
#include "pkt.h"

namespace io {
namespace {
constexpr uint32_t kSlots = 256;          // FN_SLOTS (fastpath.rs)
constexpr uint32_t kMask = kSlots - 1;
constexpr size_t kHdr = 16, kFSlot = 2048, kSlotSz = kHdr + kFSlot;
constexpr size_t kRingSz = 128 + kSlots * kSlotSz;
constexpr size_t kRxOff = 0, kTxOff = kRingSz;
constexpr int kRxBatch = 64, kTxBatch = 16;

inline volatile uint32_t* prod(char* b, size_t r) { return (volatile uint32_t*)(b + r); }
inline volatile uint32_t* cons(char* b, size_t r) { return (volatile uint32_t*)(b + r + 64); }
inline char* slot(char* b, size_t r, uint32_t i) { return b + r + 128 + (size_t)(i & kMask) * kSlotSz; }
inline char* fastmap() { uint64_t ret; __asm__ volatile("syscall" : "=a"(ret) : "a"(1000UL) : "rcx", "r11", "memory"); return (char*)ret; }

class TalariaBackend final : public Backend {
 public:
  static std::unique_ptr<Backend> create(const Config& cfg, std::string* err) {
    auto b = std::unique_ptr<TalariaBackend>(new TalariaBackend());
    b->base_ = fastmap();
    if (!b->base_ || (uintptr_t)b->base_ > 0xFFFFFFFFFFull - 1 || (int64_t)(uintptr_t)b->base_ < 0) { *err = "talaria: fastmap syscall failed (boot with talaria.fast=1, -smp 2)"; return nullptr; }
    b->dgram_ = cfg.max_datagram;
    const char* mac = std::getenv("TALARIA_MAC");
    if (!mac || !pkt::parse_mac(mac, b->tmpl_.src_mac)) { *err = "talaria: set TALARIA_MAC=aa:bb:cc:dd:ee:ff (our NIC MAC)"; return nullptr; }
    if (cfg.dpdk_ip.empty() || inet_pton(AF_INET, cfg.dpdk_ip.c_str(), &b->tmpl_.src_ip_be) != 1) { *err = "talaria: --dpdk-ip <our ip> required"; return nullptr; }
    b->tmpl_.src_port_be = pkt::htons_u16(cfg.port);
    b->tmpl_.dst_port_be = pkt::htons_u16(cfg.port);
    if (!cfg.listener) {
      if (cfg.dst_ip.empty() || inet_pton(AF_INET, cfg.dst_ip.c_str(), &b->tmpl_.dst_ip_be) != 1) { *err = "talaria: bad dst ip"; return nullptr; }
      if (cfg.dst_mac.empty() || !pkt::parse_mac(cfg.dst_mac.c_str(), b->tmpl_.dst_mac)) { *err = "talaria: --dst-mac required (static peer MAC)"; return nullptr; }
      b->have_dst_ = true;
    }
    b->rc_ = *cons(b->base_, kRxOff);
    b->tp_ = *prod(b->base_, kTxOff);
    fprintf(stderr, "io_talaria: zero-syscall rings @ %p, ip=%s port=%u listener=%d\n", (void*)b->base_, cfg.dpdk_ip.c_str(), cfg.port, (int)cfg.listener);
    return b;
  }
  bool queue(const void* payload, size_t len) override { if (!have_dst_) return false; return queue_frame(tmpl_, payload, len); }
  bool queue_to(const void* payload, size_t len, const Endpoint& to) override { return queue_frame(pkt::with_dst(tmpl_, to), payload, len); }
  void flush() override {
    if (q_ == 0) return;
    __asm__ volatile("" ::: "memory");
    *prod(base_, kTxOff) = tp_;        // publish queued slots (release: x86 TSO + compiler barrier)
    q_ = 0;
  }
  int rx(RxPacket* out, int max) override {
    // release slots consumed by the previous rx() (views were valid until now)
    if (held_ > 0) { __asm__ volatile("" ::: "memory"); *cons(base_, kRxOff) = rc_; held_ = 0; }
    if (max > kRxBatch) max = kRxBatch;
    const uint32_t rp = *prod(base_, kRxOff);
    int emitted = 0;
    while (rc_ != rp && emitted < max) {
      char* s = slot(base_, kRxOff, rc_);
      __asm__ volatile("" ::: "memory");
      uint32_t len = *(volatile uint32_t*)s; if (len > kFSlot) len = kFSlot;
      const uint8_t* frame = (const uint8_t*)(s + kHdr);
      pkt::View v;
      if (pkt::parse(frame, len, &v) && v.dst_port_be == tmpl_.src_port_be) {
        out[emitted].data = v.payload; out[emitted].len = v.len; out[emitted].from = v.from; ++emitted; ++rx_pkts_;
      } else { ++rx_filtered_; }
      ++rc_; ++held_;
    }
    return emitted;
  }
  const char* name() const override { return "talaria"; }
  void log_stats(FILE* f) const override {
    fprintf(f, "io_talaria: tx_pkts=%llu tx_drop=%llu rx_pkts=%llu rx_filtered=%llu syscalls=0\n",
            (unsigned long long)tx_pkts_, (unsigned long long)tx_drop_, (unsigned long long)rx_pkts_, (unsigned long long)rx_filtered_);
  }
 private:
  bool queue_frame(const pkt::Template& t, const void* payload, size_t len) {
    if (len > dgram_ || pkt::kHdrLen + len > kFSlot) return false;
    const uint32_t tc = *cons(base_, kTxOff);
    if (tp_ - tc >= kSlots) { flush(); if (tp_ - *cons(base_, kTxOff) >= kSlots) { ++tx_drop_; return false; } } // ring full: drop (fail fast)
    char* s = slot(base_, kTxOff, tp_);
    const size_t flen = pkt::build((uint8_t*)(s + kHdr), t, ip_id_++, payload, len);
    *(volatile uint32_t*)s = (uint32_t)flen;
    ++tp_; ++tx_pkts_;
    if (++q_ == kTxBatch) flush();
    return true;
  }
  char* base_ = nullptr;
  uint32_t rc_ = 0, tp_ = 0, held_ = 0;
  int q_ = 0;
  uint32_t dgram_ = wire::kDefaultDatagram;
  pkt::Template tmpl_{};
  bool have_dst_ = false;
  uint16_t ip_id_ = 0;
  uint64_t tx_pkts_ = 0, tx_drop_ = 0, rx_pkts_ = 0, rx_filtered_ = 0;
};
}  // namespace
std::unique_ptr<Backend> make_talaria_backend(const Config& cfg, std::string* err) { return TalariaBackend::create(cfg, err); }
}  // namespace io
