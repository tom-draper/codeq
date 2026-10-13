variable "region" {
  type    = string
  default = "eu-west-2"
}

locals {
  name = "circle"
}

# A bucket per region.
resource "aws_s3_bucket" "assets" {
  bucket = local.name
}

module "network" {
  source = "./modules/network"
}

output "bucket" {
  value = aws_s3_bucket.assets.id
}
