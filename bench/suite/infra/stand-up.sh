#!/bin/sh
# Bring the EC2 stand up: a Talaria AMI -> terraform (4 instances) -> ansible (tuning
# the Linux side) -> stand.env with the addresses.
#
#   stand-up.sh <workload> [--linux-only | --reuse-ami <ami-...>]
#   env: AWS_DEFAULT_REGION= TF_VAR_instance_type= STAND_DIR=
#
# The stand costs money and lives until stand-down.sh. All the state is in STAND_DIR,
# so it can be torn down from another session too.
set -eu
WL=${1:?workload}
MODE=${2:-}
HERE=$(cd "$(dirname "$0")" && pwd)
SUITE=$(cd "$HERE/.." && pwd)
REPO=$(cd "$SUITE/../.." && pwd)
STAND_DIR=${STAND_DIR:-$HOME/.talaria-lab/stand}
TF="$HERE/terraform"
mkdir -p "$STAND_DIR"
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}

command -v aws >/dev/null || { echo "no aws cli"; exit 1; }
command -v terraform >/dev/null || { echo "no terraform"; exit 1; }
aws sts get-caller-identity >/dev/null || { echo "the aws cli is not authenticated"; exit 1; }

AMI=""
case "$MODE" in
--linux-only) ;;
--reuse-ami)
    # BRING THE STAND UP AGAIN ON A READY AMI. Needed when only the instances change
    # (replacing a node, another type, a repeated run): building an AMI takes minutes and
    # gives nothing new. Without this mode terraform had to be called
    # by hand, and then stand.env and inventory.ini had to be rewritten by hand — and
    # that is exactly where the address mismatches appeared.
    AMI=${3:?--reuse-ami <ami-...>}
    # THE AMI MUST EXIST, AND THAT MUST BE CHECKED HERE. `stand-down.sh`
    # cleans up THE IMAGE TOO (Name=tlbench-*), so "tear down and bring up
    # on the same AMI" NEVER works — and terraform reports that with the line
    # "collecting instance settings: empty result" without a word about the image,
    # and it looks like a broken stand. Ten minutes went into working that out once.
    aws ec2 describe-images --image-ids "$AMI" --query 'Images[0].State' --output text 2>/dev/null \
        | /usr/bin/grep -q . || {
        echo "AMI $AMI DOES NOT EXIST (stand-down.sh deletes it together with the stand)."
        echo "   --reuse-ami is good only while the stand is NOT torn down; otherwise bring it up without it."
        exit 1; }
    echo "== the AMI was reused: $AMI" ;;
*)
    echo "== a Talaria AMI for workload $WL"
    AMI=$("$HERE/mk-ami.sh" "$WL" | tee "$STAND_DIR/mk-ami.log" "$STAND_DIR/mk-ami-$WL.log" | sed -n 's/^AMI_ID=//p' | tail -1)
    [ -n "$AMI" ] || { echo "the AMI was not built — see $STAND_DIR/mk-ami-$WL.log"; exit 1; }
    echo "   AMI: $AMI" ;;
esac

# THE NUMBER OF CORES COMES FROM THE WORKLOAD'S meta.env, not from terraform's default.
# Terraform's default used to be 4 and SILENTLY added `talaria.smp=4` on top of the
# baked-in `SMP=` from meta.env (mk-ami bakes it from meta.env). All the mongodb/
# mysql/pg measurements ran on 4 cores, while the Linux databases are confined to 2 (`CPUAffinity=2-3`).
# Talaria was given MORE cores — and that HURT: mongod, going by nproc=4, spawned twice
# as many threads, and the extra spinning waiters stole a physical core from the computing thread
# (SMT neighbours). At smp=2 (= meta.env = Linux) mongodb WON find/insert p50.
# Now the AMI and user-data take ONE number — from meta.env; TF_VAR_talaria_smp
# still overrides it (an A/B on the number of cores).
WL_SMP=$(. "$SUITE/workloads/$WL/meta.env"; echo "${SMP:-4}")
echo "== terraform (talaria_smp=${TF_VAR_talaria_smp:-$WL_SMP} from the meta.env of $WL)"
terraform -chdir="$TF" init -input=false >/dev/null
terraform -chdir="$TF" apply -auto-approve -input=false \
    -var "region=$AWS_DEFAULT_REGION" \
    -var "talaria_ami=$AMI" \
    -var "talaria_smp=${TF_VAR_talaria_smp:-$WL_SMP}" \
    -var "talaria_count=$([ -n "$AMI" ] && echo 2 || echo 0)" \
    ${TF_VAR_instance_type:+-var "instance_type=$TF_VAR_instance_type"} | tail -20

get() { terraform -chdir="$TF" output -json "$1" 2>/dev/null | python3 -c "
import json,sys
try:
    v = json.load(sys.stdin)
    print(v['$2'] if isinstance(v, dict) and v else '')
except Exception:
    print('')"; }

{
    echo "STAND_TYPE=$(terraform -chdir="$TF" output -raw instance_type)"
    echo "STAND_AZ=$(terraform -chdir="$TF" output -raw az)"
    echo "STAND_REGION=$AWS_DEFAULT_REGION"
    echo "STAND_KEY=$TF/$(basename "$(terraform -chdir="$TF" output -raw key_file)")"
    echo "STAND_AMI=$AMI"
    # THE KERNEL AND ANCHOR STAMPS go into stand.env, from where run-remote.sh copies them into
    # every results directory (see mk-ami.sh: one scoreboard was taken with a
    # dirty tree, and the only way to find that out was mk-ami.log).
    cat "$STAND_DIR/kernel.stamp" 2>/dev/null || echo "STAND_KERNEL_SHA=?"
    echo "STAND_LINUX_AMI=$(terraform -chdir="$TF" output -raw linux_ami 2>/dev/null)"
    echo "linux_server_ip=$(get linux_server private)"
    echo "linux_server_pub=$(get linux_server public)"
    echo "linux_loadgen_ip=$(get linux_loadgen private)"
    echo "linux_loadgen_pub=$(get linux_loadgen public)"
    echo "talaria_server_ip=$(get talaria_server private)"
    echo "talaria_server_pub=$(get talaria_server public)"
    echo "talaria_loadgen_ip=$(get talaria_loadgen private)"
    echo "talaria_loadgen_pub=$(get talaria_loadgen public)"
    # The instance ids are for taking Talaria's counters: on EC2 they are printed by
    # serial, and it can be fetched only with `aws ec2 get-console-output`.
    echo "talaria_server_id=$(terraform -chdir="$TF" output -raw talaria_server_id 2>/dev/null)"
    echo "talaria_loadgen_id=$(terraform -chdir="$TF" output -raw talaria_loadgen_id 2>/dev/null)"
} > "$STAND_DIR/stand.env"
cat "$STAND_DIR/stand.env"
. "$STAND_DIR/stand.env"

# ── The Linux side: the workload tree, the generator, the tuning ─────────────
echo "== ansible: tuning the Linux side"
A="$HERE/ansible"
TREE=$(mktemp -d)
"$SUITE/workloads/$WL/tree.sh" "$TREE" >/dev/null
# THE TREE'S ROOT MUST BE TRAVERSABLE. `mktemp -d` gives a 0700 directory, tar carries that
# mode into the `./` entry, and unarchive on the stand applies it to the chroot's root. While
# the server ran as root that did not matter; the first workload with an unprivileged
# uid (mariadbd, uid 27) hit `chroot: cannot change root directory:
# Permission denied` — and the Linux side's ceiling was 0 req/s (batch 4, mysql).
chmod 755 "$TREE"
tar czf "$A/files/tree-$WL.tar.gz" -C "$TREE" .
rm -rf "$TREE"
cp "$SUITE/tlgen/tlgen" "$A/files/tlgen"

{
    echo "[linux_stand]"
    echo "linux-server  ansible_host=$linux_server_pub  private_ip=$linux_server_ip  role=server"
    echo "linux-loadgen ansible_host=$linux_loadgen_pub private_ip=$linux_loadgen_ip role=loadgen"
    echo
    echo "[linux_stand:vars]"
    echo "ansible_user=ec2-user"
    echo "ansible_ssh_private_key_file=$STAND_KEY"
    echo "ansible_ssh_common_args=-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
    echo "workload=$WL"
} > "$A/inventory.ini"

echo "   waiting for ssh..."
for ip in "$linux_server_pub" "$linux_loadgen_pub"; do
    i=0
    while [ $i -lt 60 ]; do
        ssh -i "$STAND_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
            -o ConnectTimeout=5 ec2-user@"$ip" true 2>/dev/null && break
        i=$((i + 1)); sleep 5
    done
done
# What to run on the Linux side is taken FROM THE SAME spec by which the
# Linux guest of the local stand lives (see lib/spec2exec.py).
EXEC=$(python3 "$SUITE/lib/spec2exec.py" "$SUITE/workloads/$WL/spec")
SRV_EXEC=$(echo "$EXEC" | cut -d"|" -f1)
SRV_UID=$(echo "$EXEC" | cut -d"|" -f2)
SRV_GID=$(echo "$EXEC" | cut -d"|" -f3)
SRV_CHOWN=$(echo "$EXEC" | cut -d"|" -f4)
SRV_PORT=$(. "$SUITE/workloads/$WL/meta.env"; echo "${PORT:-}")
echo "   on the Linux server: $SRV_EXEC (uid $SRV_UID)"
# VARIABLES ARE PASSED ONLY AS JSON. `ansible-playbook -e "k=v"` splits the value
# BY SPACES and makes SEVERAL variables out of it: the server's start command
# (`/usr/bin/mongod --dbpath /data/db --bind_ip_all ...`) turned into
# `server_exec=/usr/bin/mongod` plus garbage. The unit on the Linux side got a bare
# binary without a single argument — mongod came up with the default `--dbpath` and
# listened ONLY on localhost, i.e. the generator could not reach it at all.
# Silently: the unit is "active", the port is open, the log has a routine warning.
VARS=$(python3 -c "
import json, sys
print(json.dumps({'workload': sys.argv[1], 'server_exec': sys.argv[2],
                  'server_uid': sys.argv[3], 'server_gid': sys.argv[4],
                  'server_chown': sys.argv[5], 'server_port': sys.argv[6]}))" \
    "$WL" "$SRV_EXEC" "$SRV_UID" "$SRV_GID" "$SRV_CHOWN" "$SRV_PORT")
# THE FULL ANSIBLE LOG, NOT A TAIL. `| tail -25` left only the RECAP of a run:
# for mongoose in the suite ec2all-20260905-133810 the Linux generator's stand ended with
# `failed=1`, and WHICH task failed could no longer be learned from the log. The full
# output goes to a file next to the stand, the tail to the console; a failure on any host
# brings the bring-up down rather than silently continuing the run on a half-built stand.
# THE PLAYBOOK IS RETRIED ON A DROP. The control machine goes through a VPN that
# reconnects and breaks ssh sessions (`UNREACHABLE ... Data could not be sent`,
# `Timeout waiting for privilege escalation prompt`); the playbook is idempotent
# (sysctl, packages, units), so a drop is a reason to run it again rather than
# to lose the stand together with the AMI already built: up to 3 attempts, a 20 s pause.
ANS_LOG="$STAND_DIR/ansible-$WL.log"
ans_ok=0
for attempt in 1 2 3; do
    (cd "$A" && ansible-playbook stand.yml -e "$VARS") > "$ANS_LOG" 2>&1 || true
    if /usr/bin/grep -a -E 'failed=[1-9]|unreachable=[1-9]' "$ANS_LOG" >/dev/null; then
        echo "== ansible: attempt $attempt did not pass:"
        /usr/bin/grep -a -A3 'fatal:' "$ANS_LOG" | head -12
        cp "$ANS_LOG" "$ANS_LOG.$attempt"
        sleep 20
    else ans_ok=1; break; fi
done
tail -25 "$ANS_LOG"
if [ "$ans_ok" != 1 ]; then
    echo "== ansible: REFUSAL on the stand after 3 attempts (the full log: $ANS_LOG, attempts $ANS_LOG.N):"
    /usr/bin/grep -a -B2 -A12 'fatal:' "$ANS_LOG" | head -60
    exit 1
fi

# ── The Talaria side: THE ROLE according to the serial console ───────────────
# RECAP: a postgresql run `ec2all-20260906-110825` spent half an hour in vain —
# the Talaria generator lost the first DHCP DISCOVER, fell back to a static address (`dhcp TIMEOUT`),
# did not read its user-data and came up with the default role, i.e. as a SECOND
# SERVER; the harness honestly waited for `tlgen --serve` for 600 s per point. The role is visible in the
# first lines of serial: the manifest knob set to `/loadgen.manifest`. The console
# on Nitro is published with a delay (up to a couple of minutes), but the ansible above took
# longer — we wait up to 3 minutes and fail the bring-up if the role is wrong or the kernel fell back to
# a static address: a stand without a generator is torn down at once rather than in an hour.
# A ROLE THAT DID NOT TAKE IS A REASON TO REBOOT THE NODE, NOT TO DESTROY THE STAND.
#
# The role arrives as a cmdline tail from EC2 user-data, and that is read over IMDS. IMDS
# does not always answer: once a server got `imds: no token obtained in
# 157 s (15 attempts)` — the kernel alive, DHCP in place, the neighbour on the same stand having got its role.
# The stand used to be destroyed entirely for that: twenty minutes and a dollar for someone else's
# hiccup. One reboot costs a minute and a half and fixes exactly this case.
if ! "$HERE/talaria-role.sh" "$STAND_DIR" "$talaria_server_id" "$talaria_loadgen_id"; then
    echo "== the role did not take — REBOOTING the Talaria nodes and checking again"
    aws ec2 reboot-instances --instance-ids "$talaria_server_id" "$talaria_loadgen_id" >/dev/null 2>&1 || true
    sleep 120
    "$HERE/talaria-role.sh" "$STAND_DIR" "$talaria_server_id" "$talaria_loadgen_id" || exit 1
fi

echo
echo "== the stand is up. Run it with:  bench/suite/run-remote.sh $WL"
echo "== TEAR IT DOWN WITH:             bench/suite/infra/stand-down.sh"
