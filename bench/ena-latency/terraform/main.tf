# ENA latency (bench/ena-latency): 2 metal instances in a cluster placement
# group, each with a secondary private IP (the Talaria guest's address in the VPC, no NAT).
# A tfstate separate from infra/aws (both stands can be kept in parallel).
terraform {
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 6.0" }
    tls = { source = "hashicorp/tls", version = "~> 4.0" }
  }
}

variable "region" { default = "ap-northeast-1" } # vCPU quota 256
variable "instance_type" { default = "c6a.metal" }
variable "ssh_cidr" { default = "0.0.0.0/0" }

provider "aws" { region = var.region }

resource "tls_private_key" "ena" { algorithm = "ED25519" }
resource "aws_key_pair" "ena" {
  key_name   = "talaria-ena"
  public_key = tls_private_key.ena.public_key_openssh
}
resource "local_file" "pem" {
  content         = tls_private_key.ena.private_key_openssh
  filename        = "${path.module}/talaria-ena.pem"
  file_permission = "0600"
}

data "aws_vpc" "default" { default = true }
data "aws_subnets" "default" {
  filter {
    name   = "vpc-id"
    values = [data.aws_vpc.default.id]
  }
}

resource "aws_placement_group" "ena" {
  name     = "talaria-ena"
  strategy = "cluster" # the minimal network distance between instances
}

resource "aws_security_group" "ena" {
  name_prefix = "talaria-ena-"
  vpc_id      = data.aws_vpc.default.id
  ingress {
    from_port   = 22
    to_port     = 22
    protocol    = "tcp"
    cidr_blocks = [var.ssh_cidr]
  }
  ingress { # all traffic between the stand's instances (guest IPs included)
    from_port = 0
    to_port   = 0
    protocol  = "-1"
    self      = true
  }
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

data "aws_ami" "al2023" {
  most_recent = true
  owners      = ["amazon"]
  filter {
    name   = "name"
    values = ["al2023-ami-2023*-x86_64"]
  }
}

resource "aws_instance" "node" {
  count                  = 2
  ami                    = data.aws_ami.al2023.id
  instance_type          = var.instance_type
  key_name               = aws_key_pair.ena.key_name
  subnet_id              = data.aws_subnets.default.ids[0]
  vpc_security_group_ids = [aws_security_group.ena.id]
  placement_group        = aws_placement_group.ena.name

  root_block_device {
    volume_size = 40
    volume_type = "gp3"
  }
  tags = {
    Name    = "talaria-ena-${count.index}"
    Project = "talaria"
    Purpose = "ena-latency bench, destroy after use"
  }
}

# The secondary private IP (the Talaria guest's address in the VPC) is assigned by
# orchestrate.sh via aws ec2 assign-private-ip-addresses (dynamics are simpler than in tf).

output "public_ips" { value = aws_instance.node[*].public_ip }
output "private_ips" { value = aws_instance.node[*].private_ip }
output "eni_ids" { value = aws_instance.node[*].primary_network_interface_id }
