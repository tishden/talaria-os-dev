terraform {
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 6.0" }
    tls = { source = "hashicorp/tls", version = "~> 4.0" }
  }
}

variable "region" {
  default = "us-east-1"
}
variable "instance_type" {
  # Cheapest sensible x86 metal; 4.5 GHz Cascade Lake — good latency stand.
  # The arm64 KVM stand (docs/ARM64-PLAN.md §9) is `c7g.metal` with `arch = "arm64"`:
  # Graviton3, 64 cores, no SMT, the only way to get /dev/kvm on arm64 EC2.
  default = "m5zn.metal"
}
variable "arch" {
  # The AMI's architecture: "x86_64" or "arm64". It must match the instance type, and
  # terraform cannot tell — an arm64 AMI on an x86 type fails at launch, not here.
  default = "x86_64"
}
variable "ssh_cidr" {
  default = "0.0.0.0/0"
}

provider "aws" {
  region = var.region
}

resource "tls_private_key" "stand" {
  algorithm = "ED25519"
}

resource "aws_key_pair" "stand" {
  key_name   = "talaria-stand"
  public_key = tls_private_key.stand.public_key_openssh
}

resource "local_file" "pem" {
  content         = tls_private_key.stand.private_key_openssh
  filename        = "${path.module}/talaria-stand.pem"
  file_permission = "0600"
}

data "aws_vpc" "default" {
  default = true
}

data "aws_subnets" "default" {
  filter {
    name   = "vpc-id"
    values = [data.aws_vpc.default.id]
  }
}

resource "aws_security_group" "stand" {
  name_prefix = "talaria-stand-"
  vpc_id      = data.aws_vpc.default.id

  ingress {
    from_port   = 22
    to_port     = 22
    protocol    = "tcp"
    cidr_blocks = [var.ssh_cidr]
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
    values = ["al2023-ami-2023*-${var.arch}"]
  }
}

resource "aws_instance" "stand" {
  ami                    = data.aws_ami.al2023.id
  instance_type          = var.instance_type
  key_name               = aws_key_pair.stand.key_name
  subnet_id              = data.aws_subnets.default.ids[0]
  vpc_security_group_ids = [aws_security_group.stand.id]

  root_block_device {
    volume_size = 40
    volume_type = "gp3"
  }

  tags = {
    Name    = "talaria-stand"
    Project = "talaria"
    Purpose = "poc-bench, destroy after use"
  }
}

output "public_ip" {
  value = aws_instance.stand.public_ip
}
