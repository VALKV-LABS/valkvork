//! A small AWS client for the controller's block volume driver: SigV4,
//! the EC2 Query API, and the `aws-ebs` driver (docs/block-volumes.md).

pub mod client;
pub mod ec2;
pub mod sigv4;
pub mod xml;
