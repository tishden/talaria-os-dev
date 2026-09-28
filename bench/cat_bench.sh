#!/bin/bash
# m5zn stand: checking isolcpus (tail -> plateau) and CAT (LLC isolation).
# Run as root. SYNTH — path to the static binary.
SYNTH=${SYNTH:-/opt/talaria/synth}
LAT=${LAT:-8}      # isolated core for latency
HOG=${HOG:-9}      # isolated core for the hog
NONISO=${NONISO:-0} # non-isolated core (housekeeping)
WS=${WS:-4096}     # latency working set, KB
IT=${IT:-3000000}

echo "== A. latency ALONE on isolated core $LAT (reference, tail plateau) =="
$SYNTH latency $LAT $WS $IT

echo "== A'. latency ALONE on NON-isolated core $NONISO (scheduler noise) =="
$SYNTH latency $NONISO $WS $IT

echo "== B. latency + LLC hog, WITHOUT CAT (the cache gets flushed) =="
$SYNTH hog $HOG 256 >/dev/null 2>&1 & HP=$!
sleep 1; $SYNTH latency $LAT $WS $IT; kill $HP 2>/dev/null; wait $HP 2>/dev/null

echo "== C. latency + LLC hog, WITH CAT (LLC partitioned) =="
mount -t resctrl resctrl /sys/fs/resctrl 2>/dev/null
CBM=$(cat /sys/fs/resctrl/info/L3/cbm_mask 2>/dev/null)
NBITS=$(cat /sys/fs/resctrl/info/L3/max_cbm_len 2>/dev/null || echo 0)
if [ -z "$CBM" ]; then echo "CAT unavailable (no resctrl/L3) — skipping C"; exit 0; fi
echo "L3 cbm_mask=$CBM ways=$NBITS"
mkdir -p /sys/fs/resctrl/lat /sys/fs/resctrl/hog
echo $LAT > /sys/fs/resctrl/lat/cpus_list
echo $HOG > /sys/fs/resctrl/hog/cpus_list
# latency gets the upper ~4 ways exclusively, hog the lower, non-overlapping ones
if [ "$NBITS" -ge 11 ]; then LATM="0x780"; HOGM="0x07f"; else LATM="0xc"; HOGM="0x3"; fi
echo "L3:0=$LATM" > /sys/fs/resctrl/lat/schemata
echo "L3:0=$HOGM" > /sys/fs/resctrl/hog/schemata
echo "lat schemata: $(cat /sys/fs/resctrl/lat/schemata | tr -d '\n')  |  hog: $(cat /sys/fs/resctrl/hog/schemata | tr -d '\n')"
$SYNTH hog $HOG 256 >/dev/null 2>&1 & HP=$!
sleep 1; $SYNTH latency $LAT $WS $IT; kill $HP 2>/dev/null; wait $HP 2>/dev/null
# cleanup
echo 0 > /sys/fs/resctrl/lat/cpus_list 2>/dev/null; echo 0 > /sys/fs/resctrl/hog/cpus_list 2>/dev/null
rmdir /sys/fs/resctrl/lat /sys/fs/resctrl/hog 2>/dev/null
echo DONE
