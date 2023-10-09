use aws_sdk_ec2::types::InstanceType;

/// Defines an instance that can be launched via [`Aws::create_ec2_instance`]
pub struct Ec2InstanceDefinition {
    pub(crate) instance_type: InstanceType,
    pub(crate) volume_size_gb: u32,
    pub(crate) network_interface_count: u32,
}

impl Ec2InstanceDefinition {
    /// Start defining an instance with the specified instance type
    pub fn new(instance_type: InstanceType) -> Self {
        Ec2InstanceDefinition {
            instance_type,
            volume_size_gb: 8,
            network_interface_count: 1,
        }
    }

    // Set instance to have a root volume of the specified size.
    // Defaults to 8GB.
    pub fn volume_size_gigabytes(mut self, size_gb: u32) -> Self {
        self.volume_size_gb = size_gb;
        self
    }

    /// Sets the amount of network interfaces to use on this instance.
    /// Defaults to 1
    ///
    /// Setting this to a value other than 1 will result in the creation of an elastic ip to point at your instance.
    /// This is an unfortunate requirement of AWS ECS, instances with multiple network interfaces do not get the automatically assigned ipv4 address given to instances with 1 network interface.
    /// For most users there is a hard limit of 5 elastic ip addresses allowed at one time.
    pub fn network_interface_count(mut self, count: u32) -> Self {
        self.network_interface_count = count;
        self
    }
}
