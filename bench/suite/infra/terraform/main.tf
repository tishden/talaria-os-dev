// The stand for comparative Talaria vs Linux measurements on EC2.
//
// Four instances of ONE type in ONE cluster placement group and one zone:
//
//   linux-server    linux-loadgen        talaria-server  talaria-loadgen
//        \_______________/                      \______________/
//         the "Linux against Linux" run       the "Talaria against Talaria" run
//
// Why four rather than two with a reinstall: the comparison must run on machines
// that sit in the network identically. A cluster placement group gives close
// placement but not identical placement, and reinstalling the OS changes both the host and the neighbours.
// Four machines come up at once, and both pairs live in the same contour.
//
// The Talaria instances boot from ONE AMI and differ only in user-data
// (talaria.manifest=): the role is chosen at launch, the image is the same.

terraform {
  required_providers {
    aws  = { source = "hashicorp/aws", version = "~> 6.0" }
    tls  = { source = "hashicorp/tls", version = "~> 4.0" }
    http = { source = "hashicorp/http", version = "~> 3.4" }
  }
}

variable "region"        { default = "ap-northeast-1" }
// c6i.2xlarge (8 vCPU) is the smallest type on which the stand has something to divide:
// the housekeeping cores separate from the data plane on both sides. Four such machines
// cost about a dollar and a half an hour; that is enough for a first run,
// and for a final one the type can be raised without touching anything else.
// THE INSTANCE TYPE IS LIMITED BY THE ENA DRIVER, NOT BY A WISH TO SAVE MONEY.
// On c6i.2xlarge Talaria's ENA receive delivers ONE frame and stops (`rx=1` with
// `tx` growing, the RX CQ head not moving) — DHCP then times out and the
// machine is left without a network. On c6i.large and c6i.xlarge everything works: the DHCP
// lease is obtained, receive goes on. The difference is visible in the device itself — 8 queues and
// 12 MSI-X vectors against 4 and 8, and status 0x21 against 0x1.
// The cause was found — the network was initialised TWICE (a user-data token that
// touches the NIC reopens it) — and `infra/smoke.sh` now passes on xlarge and
// 2xlarge alike. The stand stays on c6i.xlarge because every number in
// docs/MEASUREMENTS.md was taken on it: 4 vCPU, enough for housekeeping 0-1 and
// for the data plane 2-3.
variable "instance_type" { default = "c6i.xlarge" }
// THE MACHINE: x86_64 or arm64 (a Graviton stand, docs/ARM64-PLAN.md phase 9). It picks the
// Linux side's AL2023 image; the Talaria AMI carries its own (register-image --architecture),
// and the instance type must be of the same machine (stand-up.sh passes c7g.xlarge for arm64:
// 4 vCPU as c6i.xlarge, but four physical cores — Graviton has no SMT, on either side).
variable "arch" { default = "x86_64" }
variable "talaria_ami"   { default = "" }               // from bench/suite/infra/mk-ami.sh
// PRINTING COUNTERS TO SERIAL IS OFF BY DEFAULT.
//
// The `stat:` block is fifty lines, and serial on Nitro is emulated: EVERY byte
// costs an exit to the hypervisor, and the printing happens with interrupts disabled. So
// periodic printing during a LATENCY measurement spoils exactly what is being
// measured — while the harness claimed that "both paths do not touch the workload".
// Enable it deliberately and only for analysis: `-var talaria_stat_ms=5000`; the numbers of
// such a run are good for explaining but not for a protocol.
variable "talaria_stat_ms" { default = 0 }
// EXTRA CMDLINE TOKENS (an A/B of kernel knobs on EC2): `-var talaria_extra="talaria.netirq=1"`.
// Several are separated by a space. Side B comes up from the same AMI: a knob is
// a property of the boot, not of the image (see infra/talaria-cmdline.sh, which changes
// it on a live server without rebuilding the stand).
variable "talaria_extra"   { default = "" }
// The same, but ONLY for the server (diagnostics that are inadmissible on the generator).
variable "talaria_extra_server" { default = "" }
// A GENERATOR ON TALARIA MUST SLEEP WHEN IDLE — OTHERWISE IT SPOILS ITS OWN MEASUREMENT.
//
// The fourth defect of the instrument on this line, and the most expensive: it forged A WHOLE CLASS
// of tail. The generator node spun the open loop without ever falling asleep, and the EC2
// host took a ~8 ms quantum away from it by force. Those 8 ms landed in the SERVICE latency
// (the generator did not notice the arrived answer in time) and looked like a tail of the
// Talaria SERVER. The Linux side did not have them: its generator sleeps in epoll_wait,
// and steal reaches it out of idling.
//
// Measured on one bring-up (`ev36-20260917-171944`, mongodb), the same server,
// the same hour, with ONLY the generator changed:
//   find r6920: events >=4 ms 638 -> 0 (a Linux generator), max 8601 -> 814 µs
//   find r6920: events >=4 ms 638 -> 0 (talaria.netidle=1), p99.99 592 -> 137
//   find r1384: p99.99 6236 -> 159 µs, ping (5 steps): 8 ms -> zero everywhere
// The median did not move (107.5 -> 102.1): the server is the same.
//
// So the knob is a DEFAULT and only for the generator. This is a fix of the INSTRUMENT, not of the
// system being compared: on the server it would change what we measure. Turn it off
// for an A/B of the instrument itself: `-var talaria_loadgen_idle=""`.
variable "talaria_loadgen_idle" { default = "talaria.netidle=1" }
variable "ssh_cidr"      { default = "" }               // empty -> the current address
variable "name_prefix"   { default = "tlbench" }
// The Talaria roles: which manifest to execute. The paths are inside the data image.
variable "talaria_server_manifest"  { default = "/cell.manifest" }
variable "talaria_loadgen_manifest" { default = "/loadgen.manifest" }
variable "talaria_smp"              { default = 4 }
// How many Talaria machines to bring up (0 — the Linux pair only, until the AMI is built).
variable "talaria_count" { default = 2 }

provider "aws" { region = var.region }

data "http" "myip" { url = "https://checkip.amazonaws.com" }
locals {
  ssh_cidr = var.ssh_cidr != "" ? var.ssh_cidr : "${chomp(data.http.myip.response_body)}/32"
}

resource "tls_private_key" "stand" { algorithm = "ED25519" }

resource "aws_key_pair" "stand" {
  key_name   = "${var.name_prefix}-key"
  public_key = tls_private_key.stand.public_key_openssh
}

resource "local_file" "pem" {
  content         = tls_private_key.stand.private_key_openssh
  filename        = "${path.module}/${var.name_prefix}.pem"
  file_permission = "0600"
}

data "aws_vpc" "default" { default = true }

data "aws_subnets" "default" {
  filter {
    name   = "vpc-id"
    values = [data.aws_vpc.default.id]
  }
}

// ONE zone is chosen for the whole stand: different zones are different data centres, and
// the network latency between the pairs would differ before any measurement.
data "aws_subnet" "chosen" {
  id = tolist(data.aws_subnets.default.ids)[0]
}

resource "aws_placement_group" "stand" {
  name     = "${var.name_prefix}-pg"
  strategy = "cluster"
}

resource "aws_security_group" "stand" {
  name_prefix = "${var.name_prefix}-"
  vpc_id      = data.aws_vpc.default.id

  // AWS accepts rule descriptions ONLY in ASCII (verified with `terraform plan`:
  // non-ASCII here fails apply for no visible reason), so the explanation is in a
  // comment next to it while the field itself is plain Latin.
  ingress {
    // ssh from the operator's machine
    description = "operator ssh"
    from_port   = 22
    to_port     = 22
    protocol    = "tcp"
    cidr_blocks = [local.ssh_cidr]
  }
  ingress {
    // the tlgen job receiver (:9100) and the counter server (:9999)
    description = "tlgen job receiver and stats"
    from_port   = 9000
    to_port     = 9999
    protocol    = "tcp"
    cidr_blocks = [local.ssh_cidr]
  }
  ingress {
    // everything between the stand's machines
    description = "intra-stand traffic"
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    self        = true
  }
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
  tags = { Project = "talaria-bench" }
}

data "aws_ssm_parameter" "al2023" {
  name = "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-${var.arch == "arm64" ? "arm64" : "x86_64"}"
}

// ── The Linux pair ───────────────────────────────────────────────────────────
resource "aws_instance" "linux" {
  for_each = toset(["server", "loadgen"])

  ami                         = data.aws_ssm_parameter.al2023.value
  instance_type               = var.instance_type
  key_name                    = aws_key_pair.stand.key_name
  subnet_id                   = data.aws_subnet.chosen.id
  vpc_security_group_ids      = [aws_security_group.stand.id]
  placement_group             = aws_placement_group.stand.name
  associate_public_ip_address = true

  root_block_device {
    volume_size = 30
    volume_type = "gp3"
    iops        = 3000
  }
  // Hyper-threading is not disabled: the Talaria side sees all the threads too, and
  // disabling it here would be an advantage the other side does not have.
  tags = {
    Project = "talaria-bench"
    Name    = "${var.name_prefix}-linux-${each.key}"
    Stand   = "linux"
    Role    = each.key
  }
}

// ── The Talaria pair ─────────────────────────────────────────────────────────
resource "aws_instance" "talaria" {
  for_each = var.talaria_count > 0 ? toset(["server", "loadgen"]) : toset([])

  ami                         = var.talaria_ami
  instance_type               = var.instance_type
  key_name                    = aws_key_pair.stand.key_name
  subnet_id                   = data.aws_subnet.chosen.id
  vpc_security_group_ids      = [aws_security_group.stand.id]
  placement_group             = aws_placement_group.stand.name
  associate_public_ip_address = true

  // THE VOLUME IS DECLARED EXPLICITLY AND IS THE SAME AS ON THE LINUX SIDE. Without this block the volume
  // comes from the AMI's snapshot: its size and class are whatever came out of building
  // the image rather than whatever the neighbour has. For a workload that writes a journal (any
  // database) that is a comparison of MEDIA, not of systems: exactly the same mistake had to be
  // fixed on the local stand, where the Linux guest wrote into tmpfs.
  root_block_device {
    volume_size = 30
    volume_type = "gp3"
    iops        = 3000
  }

  // The role is chosen by user-data: one image, different manifests.
  user_data = join(" ", [
    "talaria.smp=${var.talaria_smp}",
    "talaria.manifest=${each.key == "server" ? var.talaria_server_manifest : var.talaria_loadgen_manifest}",
    // The counter block in serial every ${var.talaria_stat_ms} ms: on EC2 that is
    // the only way to get them (see talaria_server_id).
    var.talaria_stat_ms > 0 ? "talaria.stat=${var.talaria_stat_ms}" : "",
    var.talaria_extra,
    // THE TAIL GOES ONLY TO THE SERVER. Diagnostics on the GENERATOR node break the instrument itself:
    // `talaria.netprof=` prints datapath summaries, and tlgen --serve on a node
    // that spins with a microsecond step stops accepting connections
    // (a lost run, diag28-mysql-105553). Setting that through
    // talaria-cmdline.sh is impossible for another reason: it does a stop/start, and
    // IMDS does not always answer afterwards — user-data is then not read at all.
    each.key == "server" ? var.talaria_extra_server : "",
    // The generator's sleep when idle (see talaria_loadgen_idle): without it the host
    // takes an 8 ms quantum away from it, and it writes that into the SERVER's tail.
    each.key == "server" ? "" : var.talaria_loadgen_idle,
  ])

  tags = {
    Project = "talaria-bench"
    Name    = "${var.name_prefix}-talaria-${each.key}"
    Stand   = "talaria"
    Role    = each.key
  }
}

output "linux_server"    { value = { public = aws_instance.linux["server"].public_ip,  private = aws_instance.linux["server"].private_ip } }
output "linux_loadgen"   { value = { public = aws_instance.linux["loadgen"].public_ip, private = aws_instance.linux["loadgen"].private_ip } }
output "talaria_server"  { value = try({ public = aws_instance.talaria["server"].public_ip,  private = aws_instance.talaria["server"].private_ip }, null) }
output "talaria_loadgen" { value = try({ public = aws_instance.talaria["loadgen"].public_ip, private = aws_instance.talaria["loadgen"].private_ip }, null) }
// THE INSTANCE ID is needed for THE COUNTERS. On the Linux side statd serves them over TCP, while
// on Talaria on EC2 there is no console at all — the `stat:` block is printed to serial, and
// it can be fetched only with `aws ec2 get-console-output` by instance id.
// Without that a run on EC2 would show WHAT but would not let one explain WHY.
output "talaria_server_id"  { value = try(aws_instance.talaria["server"].id, "") }
output "talaria_loadgen_id" { value = try(aws_instance.talaria["loadgen"].id, "") }
output "linux_ami"       { value = nonsensitive(aws_instance.linux["server"].ami) }
output "region"          { value = var.region }
output "az"              { value = data.aws_subnet.chosen.availability_zone }
output "instance_type"   { value = var.instance_type }
output "key_file"        { value = local_file.pem.filename }
